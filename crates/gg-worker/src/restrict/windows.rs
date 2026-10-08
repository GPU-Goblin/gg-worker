//! The executor's self-check on Windows (ADR-0045): before reading any input it verifies the
//! sandbox it was given (an AppContainer token without capability at low integrity, a job that
//! kills it with the worker, allows one process and bounds memory, child processes blocked) and
//! refuses to run otherwise; and the probe that tries to get out of it.

use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_GROUPS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenCapabilities, TokenIntegrityLevel,
    TokenIsAppContainer,
};
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessMitigationPolicy, OpenProcessToken, ProcessChildProcessPolicy,
};

use super::{Applied, Layers, ProbeReport, RestrictParams, check, cuda_check, gpu_check};

/// `SECURITY_MANDATORY_LOW_RID`.
const LOW_INTEGRITY: u32 = 0x1000;

/// The variables the Windows launcher sets on purpose (`appcontainer::executor_spec`).
const SANDBOX_ENV: &[&str] = &[
    "SystemRoot",
    "PATH",
    "TEMP",
    "TMP",
    // Required by Windows to create an AppContainer process; it points to the container's own
    // folder.
    "LOCALAPPDATA",
    "RUST_LOG",
    "NVIDIA_TF32_OVERRIDE",
    "CUDA_DEVICE_ORDER",
    "CUDA_CACHE_PATH",
    "__GL_SHADER_DISK_CACHE",
    "__GL_SHADER_DISK_CACHE_PATH",
    "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP",
    "MESA_SHADER_CACHE_DIR",
];

/// The process token, closed when dropped.
struct Token(HANDLE);

impl Token {
    fn open() -> Result<Token, String> {
        let mut h: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle of this process and a valid out pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } == 0 {
            return Err(format!(
                "OpenProcessToken: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Token(h))
    }

    /// The raw bytes of one piece of token information.
    fn info(&self, class: i32) -> Result<Vec<u8>, String> {
        let mut len = 0u32;
        // SAFETY: the first call only reports the size; the second fills a buffer of that size.
        unsafe {
            GetTokenInformation(self.0, class, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len.max(4) as usize];
            if GetTokenInformation(self.0, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(format!(
                    "GetTokenInformation({class}): {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(buf)
        }
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by OpenProcessToken and is closed once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// What the token says: AppContainer, number of capabilities, integrity level.
fn token_facts() -> Result<(bool, u32, u32), String> {
    let t = Token::open()?;
    let is_ac = t.info(TokenIsAppContainer)?;
    let is_ac = u32::from_ne_bytes([is_ac[0], is_ac[1], is_ac[2], is_ac[3]]) != 0;
    let caps = t.info(TokenCapabilities)?;
    // SAFETY: the buffer holds a TOKEN_GROUPS, as GetTokenInformation(TokenCapabilities) writes.
    let caps = unsafe { (*(caps.as_ptr() as *const TOKEN_GROUPS)).GroupCount };
    let label = t.info(TokenIntegrityLevel)?;
    // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points inside the same buffer,
    // alive for this block; its last sub-authority is the integrity level.
    let level = unsafe {
        let l = &*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = l.Label.Sid;
        let n = *GetSidSubAuthorityCount(sid);
        *GetSidSubAuthority(sid, u32::from(n.saturating_sub(1)))
    };
    Ok((is_ac, caps, level))
}

/// The limits of the job this process is in, if any.
fn job_limits() -> Result<Option<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>, String> {
    // SAFETY: the pseudo-handle of this process; a null job handle queries the process's job;
    // the structure is plain data passed with its size.
    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) == 0 {
            return Err(format!(
                "IsProcessInJob: {}",
                std::io::Error::last_os_error()
            ));
        }
        if in_job == 0 {
            return Ok(None);
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        if QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(format!(
                "QueryInformationJobObject: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Some(info))
    }
}

/// Whether creating child processes is blocked for this process.
fn children_blocked() -> Result<bool, String> {
    let mut flags = 0u32;
    // SAFETY: PROCESS_MITIGATION_CHILD_PROCESS_POLICY is one 32-bit field of flags.
    let ok = unsafe {
        GetProcessMitigationPolicy(
            GetCurrentProcess(),
            ProcessChildProcessPolicy,
            (&mut flags as *mut u32).cast(),
            std::mem::size_of::<u32>(),
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetProcessMitigationPolicy: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Bit 0: NoChildProcessCreation.
    Ok(flags & 1 != 0)
}

/// Verify the sandbox; refuse (an error) when anything is missing. `layers` only exist on Linux:
/// here everything is checked every time.
pub fn apply_layers(_params: &RestrictParams<'_>, _layers: Layers) -> Result<Applied, String> {
    let (is_ac, caps, level) = token_facts()?;
    if !is_ac {
        return Err("not running in an AppContainer".into());
    }
    if caps != 0 {
        return Err(format!(
            "the AppContainer holds {caps} capabilities, expected none"
        ));
    }
    if level > LOW_INTEGRITY {
        return Err(format!("integrity level {level:#x}, expected low"));
    }
    let job = job_limits()?.ok_or("not running in a job")?;
    let flags = job.BasicLimitInformation.LimitFlags;
    let need = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY;
    if flags & need != need
        || job.BasicLimitInformation.ActiveProcessLimit != 1
        || job.JobMemoryLimit == 0
    {
        return Err(format!(
            "the job's limits are incomplete (flags {flags:#x}, processes {}, memory {})",
            job.BasicLimitInformation.ActiveProcessLimit, job.JobMemoryLimit
        ));
    }
    if !children_blocked()? {
        return Err("child process creation is not blocked".into());
    }
    Ok(Applied {
        landlock: "appcontainer".into(),
        no_new_privs: false,
        seccomp: false,
        rlimits: vec![
            format!("job memory {}", job.JobMemoryLimit),
            "one process".into(),
            "child processes blocked".into(),
        ],
    })
}

/// Try to get out of the sandbox, as the Linux probe does; every check must hold.
pub fn probe(
    bundle: &Path,
    canary_port: u16,
    canary_file: &Path,
    layers: Option<Layers>,
    gpu_init: Option<usize>,
    cuda_init: Option<usize>,
    memory_limit: Option<u64>,
) -> ProbeReport {
    let mut checks = Vec::new();
    let params = RestrictParams {
        bundle,
        max_address_space: None,
        max_file_bytes: 64 << 20,
        kernel_cache: None,
        session: false,
    };
    let (applied, apply_error) = match layers.map(|l| apply_layers(&params, l)) {
        Some(Ok(a)) => (Some(a), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    if let Some(device_index) = gpu_init {
        checks.push(gpu_check(device_index));
    }
    if let Some(device_index) = cuda_init {
        checks.push(cuda_check(device_index));
    }
    checks.push(match token_facts() {
        Ok((ac, caps, level)) => check(
            "appcontainer_token",
            ac && caps == 0 && level <= LOW_INTEGRITY,
            format!("AppContainer {ac}, {caps} capabilities, integrity {level:#x}"),
        ),
        Err(e) => check("appcontainer_token", false, e),
    });
    checks.push(match job_limits() {
        Ok(Some(j)) => check(
            "job_limits",
            j.BasicLimitInformation.ActiveProcessLimit == 1 && j.JobMemoryLimit > 0,
            format!(
                "flags {:#x}, processes {}, memory {}",
                j.BasicLimitInformation.LimitFlags,
                j.BasicLimitInformation.ActiveProcessLimit,
                j.JobMemoryLimit
            ),
        ),
        Ok(None) => check("job_limits", false, "not in a job"),
        Err(e) => check("job_limits", false, e),
    });
    // The worker listens on canary_port on loopback: an AppContainer without capability cannot
    // reach it.
    let local = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], canary_port)),
        Duration::from_millis(500),
    );
    checks.push(check(
        "network_isolated",
        local.is_err(),
        match &local {
            Ok(_) => "connected to the worker's canary port: loopback is NOT isolated".to_owned(),
            Err(e) => format!("connect to canary port failed as expected: {e}"),
        },
    ));
    let remote = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([1, 1, 1, 1], 443)),
        Duration::from_secs(2),
    );
    checks.push(check(
        "internet_isolated",
        remote.is_err(),
        match &remote {
            Ok(_) => "connected to 1.1.1.1:443: the network is NOT isolated".to_owned(),
            Err(e) => format!("connect to 1.1.1.1:443 failed: {e}"),
        },
    ));
    let canary = std::fs::read(canary_file);
    checks.push(check(
        "profile_unreadable",
        canary.is_err(),
        match &canary {
            Ok(_) => "a file of the user's profile was readable".to_owned(),
            Err(e) => format!("user's canary unreadable: {e}"),
        },
    ));
    let system = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\Windows".into())
        .join("gg-probe-write");
    let sys_write = std::fs::write(&system, b"x");
    checks.push(check(
        "system_read_only",
        sys_write.is_err(),
        sys_write
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("wrote {}", system.display())),
    ));
    let bundle_write = std::fs::write(bundle.join("gg-probe-write"), b"x");
    let _ = std::fs::remove_file(bundle.join("gg-probe-write"));
    checks.push(check(
        "bundle_root_read_only",
        bundle_write.is_err(),
        bundle_write
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "wrote into the bundle root".into()),
    ));
    let out = bundle.join("outputs").join("gg-probe-write");
    let out_write = std::fs::write(&out, b"x");
    let _ = std::fs::remove_file(&out);
    checks.push(check(
        "outputs_writable",
        out_write.is_ok(),
        out_write
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".into()),
    ));
    let child = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "exit 0"])
        .status();
    checks.push(check(
        "child_process_denied",
        child.is_err(),
        match &child {
            Ok(s) => format!("cmd.exe started ({s})"),
            Err(e) => e.to_string(),
        },
    ));
    if let Some(limit) = memory_limit {
        // Committing more than the job allows must fail (and not abort: try_reserve).
        let mut v: Vec<u8> = Vec::new();
        let over = v.try_reserve_exact(usize::try_from(limit + (256 << 20)).unwrap_or(usize::MAX));
        checks.push(check(
            "memory_limited",
            over.is_err(),
            match over {
                Ok(()) => format!("committed {} bytes past the limit", 256 << 20),
                Err(e) => format!("allocation past the job's limit refused: {e}"),
            },
        ));
    }
    let unexpected: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| !SANDBOX_ENV.iter().any(|s| s.eq_ignore_ascii_case(k)))
        .collect();
    checks.push(check(
        "environment_clean",
        unexpected.is_empty(),
        if unexpected.is_empty() {
            "only the variables the sandbox sets".to_owned()
        } else {
            format!("unexpected variables: {}", unexpected.join(", "))
        },
    ));
    if applied.is_none() {
        checks.push(check(
            "sandbox_enforced",
            false,
            apply_error
                .clone()
                .unwrap_or_else(|| "restrictions not checked".into()),
        ));
    }
    ProbeReport {
        applied,
        apply_error,
        checks,
    }
}
