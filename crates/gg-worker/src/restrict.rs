//! Executor self-restriction (applied inside the sandbox before any input is read):
//! Landlock filesystem rules, a seccomp deny-list and resource limits. Also the sandbox probe used
//! by `gg-worker doctor` to measure what is really enforced.

use std::collections::BTreeMap;
use std::path::Path;

use landlock::{
    ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    path_beneath_rules,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use serde::{Deserialize, Serialize};

/// What was enforced.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Applied {
    /// Landlock ruleset status: `fully_enforced`, `partially_enforced`, `not_enforced`, `error: …`.
    pub landlock: String,
    /// `no_new_privs` set.
    pub no_new_privs: bool,
    /// seccomp filter installed.
    pub seccomp: bool,
    /// rlimits applied (`RLIMIT_FSIZE`, `RLIMIT_NOFILE`, optionally `RLIMIT_AS`).
    pub rlimits: Vec<String>,
}

/// Environment variables the sandbox sets on purpose (`sandbox::plan_with`); any other one found
/// inside fails `doctor`.
pub const SANDBOX_ENV: &[&str] = &[
    "RUST_LOG",
    "HOME",
    "PATH",
    "PWD",
    "LD_LIBRARY_PATH",
    "NVIDIA_TF32_OVERRIDE",
    "CUDA_CACHE_PATH",
    "__GL_SHADER_DISK_CACHE",
    "__GL_SHADER_DISK_CACHE_PATH",
    "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP",
    "MESA_SHADER_CACHE_DIR",
];

/// Restriction parameters.
#[derive(Clone, Debug)]
pub struct RestrictParams<'a> {
    /// Bundle root (read-only except `outputs/`).
    pub bundle: &'a Path,
    /// Address-space limit in bytes (skipped for GPU engines: driver mappings need large VA).
    pub max_address_space: Option<u64>,
    /// Maximum size of any file the executor may create.
    pub max_file_bytes: u64,
    /// Compiled GPU kernels kept between tasks, read-write (ADR-0032); `None`: no cache.
    pub kernel_cache: Option<&'a Path>,
    /// An executor session (ADR-0034): `bundle` is the directory of the session's attempt
    /// bundles, writable as a whole, since each attempt's `outputs/` is not known in advance.
    pub session: bool,
}

/// Apply Landlock: read-only system paths and bundle, read-write `outputs/` and `/tmp`, read-write
/// `/dev` (device ioctls need `O_RDWR`; inside the sandbox `/dev` only holds the allowed nodes).
/// `kernel_cache`: the compiled-kernel cache (ADR-0032), read-write when present.
pub fn apply_landlock(
    bundle: &Path,
    kernel_cache: Option<&Path>,
    session: bool,
) -> Result<(RulesetStatus, bool), String> {
    let abi = ABI::V5;
    let libtorch = Path::new(crate::bundle::SANDBOX_LIBTORCH_DIR);
    let ro: Vec<&Path> = ["/usr", "/lib", "/lib64", "/bin", "/etc", "/proc", "/sys"]
        .iter()
        .map(Path::new)
        .chain(std::iter::once(bundle))
        .chain(libtorch.exists().then_some(libtorch))
        .collect();
    // The process's own threads (`/proc/<pid>/task`): the NVIDIA CUDA driver names its helper
    // threads by writing their `comm` there, and its initialization fails when it cannot
    // ("CUDA is not available"). Nothing else in /proc is writable.
    // SAFETY: getpid has no preconditions.
    let own_tasks = Path::new("/proc")
        .join(unsafe { libc::getpid() }.to_string())
        .join("task");
    let rw: Vec<std::path::PathBuf> = [
        if session {
            bundle.to_path_buf()
        } else {
            bundle.join("outputs")
        },
        Path::new("/tmp").to_path_buf(),
        Path::new("/dev").to_path_buf(),
    ]
    .into_iter()
    .chain(kernel_cache.map(Path::to_path_buf))
    .chain(own_tasks.exists().then_some(own_tasks))
    .collect();
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules(&ro, AccessFs::from_read(abi)))
        .map_err(|e| e.to_string())?
        .add_rules(path_beneath_rules(&rw, AccessFs::from_all(abi)))
        .map_err(|e| e.to_string())?
        .restrict_self()
        .map_err(|e| e.to_string())?;
    Ok((status.ruleset, status.no_new_privs))
}

/// Syscalls denied outright (EPERM). Socket *operations* stay allowed because `socket()` is limited
/// to `AF_UNIX` (the NVIDIA driver and the Vulkan loader use unix sockets internally); network
/// isolation itself comes from the empty network namespace of the sandbox.
pub const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_keyctl,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    libc::SYS_personality,
    libc::SYS_acct,
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
    libc::SYS_mknod,
    libc::SYS_mknodat,
    libc::SYS_setuid,
    libc::SYS_setgid,
    libc::SYS_setreuid,
    libc::SYS_setregid,
    libc::SYS_setresuid,
    libc::SYS_setresgid,
    libc::SYS_setfsuid,
    libc::SYS_setfsgid,
    libc::SYS_capset,
    libc::SYS_open_by_handle_at,
    libc::SYS_fanotify_init,
    libc::SYS_ioperm,
    libc::SYS_iopl,
    libc::SYS_vhangup,
    libc::SYS_syslog,
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_fork,
    libc::SYS_vfork,
];

/// Build the seccomp program: deny [`DENIED_SYSCALLS`] and any `socket()` whose domain is not
/// `AF_UNIX`; everything else is allowed (deny-list: compute libraries need many syscalls).
pub fn build_seccomp() -> Result<BpfProgram, String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for sc in DENIED_SYSCALLS {
        rules.insert(*sc, vec![]);
    }
    // socket(domain, ...) with domain != AF_UNIX
    let not_unix = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .map_err(|e| e.to_string())?,
    ])
    .map_err(|e| e.to_string())?;
    rules.insert(libc::SYS_socket, vec![not_unix]);
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        TargetArch::x86_64,
    )
    .map_err(|e| e.to_string())?;
    let prog: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| e.to_string())?;
    Ok(prog)
}

/// Install the seccomp filter on all threads.
pub fn apply_seccomp() -> Result<(), String> {
    let prog = build_seccomp()?;
    seccompiler::apply_filter_all_threads(&prog).map_err(|e| e.to_string())
}

fn set_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> Result<(), String> {
    let lim = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: setrlimit with a valid pointer to an initialized rlimit struct.
    let r = unsafe { libc::setrlimit(resource, &lim) };
    if r != 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

/// Apply resource limits.
pub fn apply_rlimits(params: &RestrictParams<'_>) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    set_rlimit(libc::RLIMIT_FSIZE, params.max_file_bytes)?;
    applied.push(format!("RLIMIT_FSIZE={}", params.max_file_bytes));
    set_rlimit(libc::RLIMIT_NOFILE, 256)?;
    applied.push("RLIMIT_NOFILE=256".into());
    set_rlimit(libc::RLIMIT_CORE, 0)?;
    applied.push("RLIMIT_CORE=0".into());
    if let Some(as_bytes) = params.max_address_space {
        set_rlimit(libc::RLIMIT_AS, as_bytes)?;
        applied.push(format!("RLIMIT_AS={as_bytes}"));
    }
    Ok(applied)
}

/// Which layers to apply (all by default; partial sets exist only for the diagnostic probe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layers {
    /// Resource limits.
    pub rlimits: bool,
    /// Landlock filesystem rules.
    pub landlock: bool,
    /// seccomp syscall filter.
    pub seccomp: bool,
}

impl Layers {
    /// Everything.
    pub const ALL: Layers = Layers {
        rlimits: true,
        landlock: true,
        seccomp: true,
    };
}

/// Apply everything, in order: rlimits, Landlock, seccomp. Must run before any thread is spawned.
pub fn apply_all(params: &RestrictParams<'_>) -> Result<Applied, String> {
    apply_layers(params, Layers::ALL)
}

/// Apply the selected layers (diagnostics only; tasks always use [`apply_all`]).
pub fn apply_layers(params: &RestrictParams<'_>, layers: Layers) -> Result<Applied, String> {
    let rlimits = if layers.rlimits {
        apply_rlimits(params)?
    } else {
        Vec::new()
    };
    let (landlock, no_new_privs) = if layers.landlock {
        let (status, nnp) = apply_landlock(params.bundle, params.kernel_cache, params.session)?;
        (
            match status {
                RulesetStatus::FullyEnforced => "fully_enforced".to_owned(),
                RulesetStatus::PartiallyEnforced => "partially_enforced".to_owned(),
                RulesetStatus::NotEnforced => "not_enforced".to_owned(),
            },
            nnp,
        )
    } else {
        ("skipped".to_owned(), false)
    };
    if layers.seccomp {
        apply_seccomp()?;
    }
    Ok(Applied {
        landlock,
        no_new_privs,
        seccomp: layers.seccomp,
        rlimits,
    })
}

/// Result of one probe check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeCheck {
    /// Name.
    pub name: String,
    /// Whether the protection held.
    pub ok: bool,
    /// Detail.
    pub detail: String,
}

/// Probe report printed by `gg-executor probe`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeReport {
    /// What was applied.
    pub applied: Option<Applied>,
    /// Error while applying, if any.
    pub apply_error: Option<String>,
    /// Checks.
    pub checks: Vec<ProbeCheck>,
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> ProbeCheck {
    ProbeCheck {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

#[cfg(feature = "gpu")]
fn gpu_check(device_index: usize) -> ProbeCheck {
    match gg_compute::engine::gpu::init_vulkan(device_index) {
        Ok((_, info)) => check("gpu_init_restricted", true, info.device_name),
        Err(e) => check("gpu_init_restricted", false, e.to_string()),
    }
}

#[cfg(not(feature = "gpu"))]
fn gpu_check(_device_index: usize) -> ProbeCheck {
    check(
        "gpu_init_restricted",
        false,
        "this executor was built without GPU support (feature `gpu`)",
    )
}

#[cfg(feature = "cuda")]
fn cuda_check(device_index: usize) -> ProbeCheck {
    match gg_compute::engine::cuda::check(device_index) {
        Ok(info) => check("cuda_init_restricted", true, info.device_name),
        Err(e) => check("cuda_init_restricted", false, e.to_string()),
    }
}

#[cfg(not(feature = "cuda"))]
fn cuda_check(_device_index: usize) -> ProbeCheck {
    check(
        "cuda_init_restricted",
        false,
        "this executor was built without the native CUDA engine (feature `cuda`)",
    )
}

fn seccomp_mode() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Seccomp:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Run the probe: apply the selected restriction layers, then try to break out. `canary_port` is a
/// TCP port the agent listens on outside the sandbox; `canary_file` a readable host file outside
/// the binds. `gpu_init` additionally initializes the Vulkan device under restriction, `cuda_init`
/// the native CUDA engine (ADR-0033).
pub fn probe(
    bundle: &Path,
    canary_port: u16,
    canary_file: &Path,
    layers: Option<Layers>,
    gpu_init: Option<usize>,
    cuda_init: Option<usize>,
) -> ProbeReport {
    let mut checks = Vec::new();
    let (applied, apply_error) = if let Some(layers) = layers {
        match apply_layers(
            &RestrictParams {
                bundle,
                max_address_space: None,
                max_file_bytes: 64 << 20,
                kernel_cache: None,
                session: false,
            },
            layers,
        ) {
            Ok(a) => (Some(a), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    if let Some(device_index) = gpu_init {
        checks.push(gpu_check(device_index));
    }
    if let Some(device_index) = cuda_init {
        checks.push(cuda_check(device_index));
    }
    // Namespaces: our pid is tiny in a fresh pid namespace and /proc shows almost no processes.
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() };
    let procs = std::fs::read_dir("/proc")
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .ok()
                    .and_then(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
                    .is_some()
            })
            .count()
        })
        .unwrap_or(9999);
    checks.push(check(
        "pid_namespace",
        pid <= 3 && procs <= 4,
        format!("pid {pid}, {procs} processes visible"),
    ));
    // Network: the agent listens on canary_port; from a new network namespace it is unreachable.
    let net = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], canary_port)),
        std::time::Duration::from_millis(500),
    );
    checks.push(check(
        "network_isolated",
        net.is_err(),
        match &net {
            Ok(_) => "connected to the agent's canary port: network is NOT isolated".to_owned(),
            Err(e) => format!("connect to canary port failed as expected: {e}"),
        },
    ));
    let interfaces = std::fs::read_to_string("/proc/net/dev")
        .map(|s| s.lines().skip(2).count())
        .unwrap_or(99);
    checks.push(check(
        "network_interfaces",
        interfaces <= 1,
        format!("{interfaces} interface(s) visible"),
    ));
    // seccomp: socket(AF_INET) must fail with EPERM once the filter is active.
    // SAFETY: plain libc call with constant arguments; the fd is closed if created.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let errno = std::io::Error::last_os_error();
    if fd >= 0 {
        // SAFETY: fd is a valid descriptor we just created.
        unsafe { libc::close(fd) };
    }
    let mode = seccomp_mode();
    checks.push(check(
        "seccomp_active",
        fd < 0 && mode == Some(2),
        format!(
            "socket(AF_INET) → {}; Seccomp mode {:?}",
            if fd < 0 {
                errno.to_string()
            } else {
                "succeeded".into()
            },
            mode
        ),
    ));
    // Filesystem: the host canary must be invisible; system paths read-only; bundle root read-only;
    // outputs writable.
    let canary = std::fs::read(canary_file);
    checks.push(check(
        "mount_isolated",
        canary.is_err(),
        match &canary {
            Ok(_) => "host canary file readable: mounts are NOT isolated".to_owned(),
            Err(e) => format!("host canary unreadable: {e}"),
        },
    ));
    let sys_write = std::fs::write("/usr/gg-probe-write", b"x");
    checks.push(check(
        "system_read_only",
        sys_write.is_err(),
        sys_write
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "wrote into /usr".into()),
    ));
    let bundle_write = std::fs::write(bundle.join("gg-probe-write"), b"x");
    let _ = std::fs::remove_file(bundle.join("gg-probe-write"));
    checks.push(check(
        "bundle_root_read_only",
        bundle_write.is_err(),
        bundle_write
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "wrote into the bundle root (Landlock not enforced)".into()),
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
    // Process: exec must be denied.
    let exec = std::process::Command::new("/bin/true").status();
    checks.push(check(
        "exec_denied",
        exec.is_err(),
        match &exec {
            Ok(s) => format!("exec succeeded ({s})"),
            Err(e) => e.to_string(),
        },
    ));
    // The environment holds only what the sandbox sets on purpose (names only in the report: a
    // value could be a secret).
    let unexpected: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| !SANDBOX_ENV.contains(&k.as_str()))
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
    // Not root.
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    checks.push(check("not_root", uid != 0, format!("uid {uid}")));
    if let Some(a) = &applied {
        checks.push(check(
            "landlock_enforced",
            a.landlock == "fully_enforced" && a.no_new_privs,
            format!("{} (no_new_privs {})", a.landlock, a.no_new_privs),
        ));
    } else {
        checks.push(check(
            "landlock_enforced",
            false,
            apply_error
                .clone()
                .unwrap_or_else(|| "restrictions not applied".into()),
        ));
    }
    ProbeReport {
        applied,
        apply_error,
        checks,
    }
}

/// cgroup v2 limits of the current process (read from `/sys/fs/cgroup`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CgroupReport {
    /// cgroup path.
    pub path: String,
    /// `memory.max` (`max` = unlimited).
    pub memory_max: String,
    /// `cpu.max`.
    pub cpu_max: String,
    /// `pids.max`.
    pub pids_max: String,
}

/// Read the cgroup limits of this process.
pub fn cgroup_limits() -> CgroupReport {
    let path = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
        })
        .unwrap_or_default();
    let read = |name: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/{name}"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "unreadable".into())
    };
    CgroupReport {
        memory_max: read("memory.max"),
        cpu_max: read("cpu.max"),
        pids_max: read("pids.max"),
        path,
    }
}
