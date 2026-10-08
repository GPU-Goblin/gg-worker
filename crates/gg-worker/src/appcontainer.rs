//! The executor's sandbox on Windows (ADR-0045): the executor runs in an AppContainer without any
//! capability (no network, loopback included; nothing of the user reachable but the directories
//! granted here), inside a Job Object (killed with the worker's handle, one process only, a memory
//! and CPU limit, no access to the desktop's clipboard, atoms or other windows), with process
//! mitigation policies set at creation (no child process, strict handle checks, no extension
//! points, no image from a remote share or with a low label).
//!
//! `std::process::Command` cannot pass these creation attributes, so this module calls
//! `CreateProcessW` itself and hands back the same pipes and a killable child. Every `unsafe`
//! block states what it relies on.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT, LocalFree,
    SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, FreeSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_CAPABILITIES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// The AppContainer profile of GPU Goblin's executors.
pub const PROFILE: &str = "GPUGoblin.Executor";

// Process and thread attributes (winbase.h), stable values of the Windows ABI.
const ATTR_HANDLE_LIST: usize = 0x0002_0002;
const ATTR_MITIGATION_POLICY: usize = 0x0002_0007;
const ATTR_SECURITY_CAPABILITIES: usize = 0x0002_0009;
const ATTR_CHILD_PROCESS_POLICY: usize = 0x0002_000E;
// Mitigation policies (winnt.h), "always on".
const STRICT_HANDLE_CHECKS: u64 = 1 << 24;
const EXTENSION_POINT_DISABLE: u64 = 1 << 32;
const IMAGE_LOAD_NO_REMOTE: u64 = 1 << 52;
const IMAGE_LOAD_NO_LOW_LABEL: u64 = 1 << 56;
const CHILD_PROCESS_RESTRICTED: u32 = 0x01;
// File access rights (winnt.h).
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
const DELETE: u32 = 0x0001_0000;
const FILE_DELETE_CHILD: u32 = 0x0000_0040;
/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;

/// Access granted to the AppContainer on a directory (and what it holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read and run: the executor's and LibTorch's directories.
    ReadExecute,
    /// Read: the bundle's inputs.
    Read,
    /// Read, write, create, delete: `outputs\`, the temporary directory, the kernel cache.
    ReadWrite,
}

impl Access {
    fn mask(self) -> u32 {
        match self {
            Access::ReadExecute => FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            Access::Read => FILE_GENERIC_READ,
            Access::ReadWrite => {
                FILE_GENERIC_READ
                    | FILE_GENERIC_WRITE
                    | FILE_GENERIC_EXECUTE
                    | DELETE
                    | FILE_DELETE_CHILD
            }
        }
    }
}

/// One launch in the sandbox.
#[derive(Clone, Debug)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory (granted separately).
    pub cwd: PathBuf,
    /// The whole environment of the executor (nothing is inherited).
    pub env: Vec<(String, OsString)>,
    /// Directories the executor may reach, and how.
    pub grants: Vec<(PathBuf, Access)>,
    /// Memory of the whole job, in bytes.
    pub memory: u64,
    /// CPU in percent of one core (100 = one core); `None`: no limit.
    pub cpu_percent: Option<u32>,
}

/// The AppContainer SID, freed when dropped.
struct Sid(PSID);

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: the SID was allocated by CreateAppContainerProfile or
        // DeriveAppContainerSidFromAppContainerName, which document FreeSid.
        unsafe {
            FreeSid(self.0);
        }
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// The profile's SID, creating the profile the first time (per user, no administrator needed).
fn profile_sid() -> io::Result<Sid> {
    let name = wide(OsStr::new(PROFILE));
    let display = wide(OsStr::new("GPU Goblin executor"));
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: null-terminated strings; no capability (null, 0); `sid` receives an allocated SID.
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            display.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        )
    };
    if hr == ALREADY_EXISTS {
        // SAFETY: as above; the profile exists, its SID is derived from its name.
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr != 0 {
            return Err(io::Error::other(format!(
                "AppContainer profile {PROFILE}: HRESULT {hr:#x}"
            )));
        }
    } else if hr != 0 {
        return Err(io::Error::other(format!(
            "AppContainer profile {PROFILE}: HRESULT {hr:#x}"
        )));
    }
    Ok(Sid(sid))
}

/// Grant `access` on `dir` (inherited by what it holds) to the AppContainer.
fn grant(sid: &Sid, dir: &Path, access: Access) -> io::Result<()> {
    let path = wide(dir.as_os_str());
    // SAFETY: GetNamedSecurityInfoW fills `dacl` (inside `sd`) and `sd`, freed with LocalFree;
    // SetEntriesInAclW allocates `new_acl`, freed with LocalFree; the trustee points to the live
    // SID; the path is null-terminated.
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let r = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if r != ERROR_SUCCESS {
            return Err(io::Error::other(format!(
                "reading the access list of {}: error {r}",
                dir.display()
            )));
        }
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.0.cast(),
        };
        // Already granted (the executor's and LibTorch's directories, the kernel cache, after the
        // first task): rewriting the list would walk every file below, for nothing.
        let mut rights = 0u32;
        if GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) == ERROR_SUCCESS
            && rights & access.mask() == access.mask()
        {
            LocalFree(sd.cast());
            return Ok(());
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access.mask(),
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: trustee,
        };
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let r = SetEntriesInAclW(1, &ea, dacl, &mut new_acl);
        if r != ERROR_SUCCESS {
            LocalFree(sd.cast());
            return Err(io::Error::other(format!(
                "building the access list of {}: error {r}",
                dir.display()
            )));
        }
        let r = SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null(),
        );
        LocalFree(new_acl.cast());
        LocalFree(sd.cast());
        if r == ERROR_ACCESS_DENIED {
            return Err(io::Error::other(format!(
                "cannot grant the executor access to {}: this user does not own it (the programs \
                 and the worker's data must be in folders the user owns, as the installer puts \
                 them)",
                dir.display()
            )));
        } else if r != ERROR_SUCCESS {
            return Err(io::Error::other(format!(
                "granting the executor access to {}: error {r}",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// The job: killed with its last handle, one process, memory and CPU bounded, no desktop access.
fn job(memory: u64, cpu_percent: Option<u32>) -> io::Result<OwnedHandle> {
    // SAFETY: an anonymous job; the handle is owned from here on.
    let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if h.is_null() {
        return Err(last_error("CreateJobObjectW"));
    }
    // SAFETY: `h` is a fresh, valid handle owned by nobody else.
    let job = unsafe { OwnedHandle::from_raw_handle(h as RawHandle) };
    // SAFETY: each structure is zero-initialized (valid for these plain C structs), filled, and
    // passed with its exact size.
    unsafe {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.JobMemoryLimit = usize::try_from(memory).unwrap_or(usize::MAX);
        if SetInformationJobObject(
            h,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(last_error("job limits"));
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };
        if SetInformationJobObject(
            h,
            JobObjectBasicUIRestrictions,
            (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            return Err(last_error("job desktop restrictions"));
        }
        if let Some(pct) = cpu_percent.filter(|p| *p > 0) {
            // CpuRate: hundredths of a percent of the whole machine.
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
            let rate = (pct.saturating_mul(100) / cores.max(1)).clamp(1, 10_000);
            let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
            cpu.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            cpu.Anonymous.CpuRate = rate;
            if SetInformationJobObject(
                h,
                JobObjectCpuRateControlInformation,
                (&cpu as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            ) == 0
            {
                return Err(last_error("job CPU limit"));
            }
        }
    }
    Ok(job)
}

/// An anonymous pipe: the end kept here is not inheritable, the child's end is.
fn pipe(child_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid out pointers; both handles are owned right after.
    unsafe {
        if CreatePipe(&mut r, &mut w, &sa, 0) == 0 {
            return Err(last_error("CreatePipe"));
        }
        let (r, w) = (
            OwnedHandle::from_raw_handle(r as RawHandle),
            OwnedHandle::from_raw_handle(w as RawHandle),
        );
        let (mine, theirs) = if child_reads { (w, r) } else { (r, w) };
        if SetHandleInformation(mine.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, 0) == 0 {
            return Err(last_error("SetHandleInformation"));
        }
        Ok((mine, theirs))
    }
}

/// Quote one argument for `CommandLineToArgvW` (the rules the C runtime and Rust follow).
fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let a: Vec<u16> = arg.encode_wide().collect();
    let needs = a.is_empty()
        || a.iter()
            .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'"'));
    if !needs {
        out.extend_from_slice(&a);
        return;
    }
    out.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for &c in &a {
        if c == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if c == u16::from(b'"') {
                out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
            } else {
                out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            }
            backslashes = 0;
            out.push(c);
        }
    }
    out.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    out.push(u16::from(b'"'));
}

fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let mut out = Vec::new();
    quote(program.as_os_str(), &mut out);
    for a in args {
        out.push(u16::from(b' '));
        quote(a, &mut out);
    }
    out.push(0);
    out
}

/// A Unicode environment block, sorted by name as Windows expects.
fn environment(env: &[(String, OsString)]) -> Vec<u16> {
    let mut vars: Vec<&(String, OsString)> = env.iter().collect();
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut out = Vec::new();
    for (k, v) in vars {
        out.extend(OsStr::new(k).encode_wide());
        out.push(u16::from(b'='));
        out.extend(v.encode_wide());
        out.push(0);
    }
    out.push(0);
    out
}

/// A sandboxed executor.
pub struct Child {
    process: OwnedHandle,
    job: OwnedHandle,
    pub stdin: Option<tokio::fs::File>,
    pub stdout: Option<tokio::fs::File>,
    pub stderr: Option<tokio::fs::File>,
}

impl Child {
    /// Wait for the executor to end; its exit code.
    pub async fn wait(&mut self) -> io::Result<u32> {
        let h = self.process.try_clone()?;
        tokio::task::spawn_blocking(move || {
            // SAFETY: `h` is a valid process handle owned by this closure.
            unsafe {
                if WaitForSingleObject(h.as_raw_handle() as HANDLE, INFINITE) != WAIT_OBJECT_0 {
                    return Err(last_error("WaitForSingleObject"));
                }
                let mut code = 0u32;
                if GetExitCodeProcess(h.as_raw_handle() as HANDLE, &mut code) == 0 {
                    return Err(last_error("GetExitCodeProcess"));
                }
                Ok(code)
            }
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Kill every process of the job.
    pub fn kill(&self) {
        // SAFETY: the job handle is valid for the life of `self`.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle() as HANDLE, 1);
        }
    }
}

/// Start `spec` in the sandbox. The standard handles are pipes; `stdin`: whether to give the
/// executor a standard input (sessions) or none.
pub fn spawn(spec: &Spec, stdin: bool) -> io::Result<Child> {
    let sid = profile_sid()?;
    for (dir, access) in &spec.grants {
        grant(&sid, dir, *access)?;
    }
    let job = job(spec.memory, spec.cpu_percent)?;
    let (in_mine, in_theirs) = pipe(true)?;
    let (out_mine, out_theirs) = pipe(false)?;
    let (err_mine, err_theirs) = pipe(false)?;
    let inherited: Vec<HANDLE> = [&in_theirs, &out_theirs, &err_theirs]
        .iter()
        .map(|h| h.as_raw_handle() as HANDLE)
        .collect();

    let mut caps = SECURITY_CAPABILITIES {
        AppContainerSid: sid.0,
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mitigation: u64 = STRICT_HANDLE_CHECKS
        | EXTENSION_POINT_DISABLE
        | IMAGE_LOAD_NO_REMOTE
        | IMAGE_LOAD_NO_LOW_LABEL;
    let child_policy: u32 = CHILD_PROCESS_RESTRICTED;

    let mut cmd = command_line(&spec.program, &spec.args);
    let env = environment(&spec.env);
    let program = wide(spec.program.as_os_str());
    let cwd = wide(spec.cwd.as_os_str());

    // SAFETY: the attribute list is sized by the first call and initialized by the second; every
    // attribute value (`caps`, `inherited`, `mitigation`, `child_policy`) outlives CreateProcessW;
    // the list is deleted before its buffer is freed. The process is created suspended, assigned
    // to the job, then resumed: it runs no instruction outside the job. All returned handles are
    // owned or closed.
    unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 4, 0, &mut size);
        let mut buf = vec![0u8; size];
        let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, 4, 0, &mut size) == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        let set = |attr: usize, value: *const std::ffi::c_void, len: usize| {
            if UpdateProcThreadAttribute(
                list,
                0,
                attr,
                value,
                len,
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                Err(last_error("UpdateProcThreadAttribute"))
            } else {
                Ok(())
            }
        };
        let attrs = set(
            ATTR_SECURITY_CAPABILITIES,
            (&mut caps as *mut SECURITY_CAPABILITIES).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )
        .and_then(|()| {
            set(
                ATTR_HANDLE_LIST,
                inherited.as_ptr().cast(),
                inherited.len() * std::mem::size_of::<HANDLE>(),
            )
        })
        .and_then(|()| {
            set(
                ATTR_MITIGATION_POLICY,
                (&mitigation as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        })
        .and_then(|()| {
            set(
                ATTR_CHILD_PROCESS_POLICY,
                (&child_policy as *const u32).cast(),
                std::mem::size_of::<u32>(),
            )
        });
        if let Err(e) = attrs {
            DeleteProcThreadAttributeList(list);
            return Err(e);
        }
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_theirs.as_raw_handle() as HANDLE;
        si.StartupInfo.hStdOutput = out_theirs.as_raw_handle() as HANDLE;
        si.StartupInfo.hStdError = err_theirs.as_raw_handle() as HANDLE;
        si.lpAttributeList = list;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            program.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            // No console at all (its standard handles are pipes): with CREATE_NO_WINDOW Windows
            // starts a hidden console host for it, a child process the policy above forbids,
            // and the executor dies in its initialization (STATUS_DLL_INIT_FAILED, measured).
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_UNICODE_ENVIRONMENT
                | DETACHED_PROCESS
                | CREATE_SUSPENDED,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        // The error is read before anything else can overwrite it.
        let created = (ok != 0).then_some(()).ok_or_else(io::Error::last_os_error);
        DeleteProcThreadAttributeList(list);
        drop(buf);
        if let Err(e) = created {
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "starting {} in the AppContainer: {e}",
                    spec.program.display()
                ),
            ));
        }
        let process = OwnedHandle::from_raw_handle(pi.hProcess as RawHandle);
        let thread = OwnedHandle::from_raw_handle(pi.hThread as RawHandle);
        if AssignProcessToJobObject(job.as_raw_handle() as HANDLE, pi.hProcess) == 0 {
            let e = last_error("AssignProcessToJobObject");
            TerminateProcess(pi.hProcess, 1);
            return Err(e);
        }
        if ResumeThread(thread.as_raw_handle() as HANDLE) == u32::MAX {
            let e = last_error("ResumeThread");
            TerminateJobObject(job.as_raw_handle() as HANDLE, 1);
            return Err(e);
        }
        drop(thread);
        // The child's ends are closed here: the pipes end when the executor does.
        drop((in_theirs, out_theirs, err_theirs));
        let file = |h: OwnedHandle| tokio::fs::File::from_std(std::fs::File::from(h));
        Ok(Child {
            process,
            job,
            stdin: stdin.then(|| file(in_mine)),
            stdout: Some(file(out_mine)),
            stderr: Some(file(err_mine)),
        })
    }
}

/// The launch of an executor for a task bundle (`session`: a session's root of bundles, writable
/// as a whole), as `sandbox::plan_with` builds it for bubblewrap: the executor's and LibTorch's
/// directories to read and run, the bundle to read, its `outputs\` and a temporary directory to
/// write, the kernel cache (ADR-0032) to read and write; an environment built from nothing.
#[allow(clippy::too_many_arguments)]
pub fn executor_spec(
    exe: &Path,
    root: &Path,
    session: bool,
    args: Vec<OsString>,
    kernel_cache: Option<&Path>,
    libtorch: Option<&Path>,
    memory: u64,
    cpu_percent: Option<u32>,
) -> Result<Spec, String> {
    let tmp = root.join(if session { ".tmp" } else { "tmp" });
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let exe_dir = exe
        .parent()
        .ok_or("the executor has no directory")?
        .to_path_buf();
    let system_root =
        std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
    let mut grants = vec![(exe_dir.clone(), Access::ReadExecute)];
    let mut path = exe_dir.into_os_string();
    if let Some(lt) = libtorch {
        grants.push((lt.to_path_buf(), Access::ReadExecute));
        path.push(";");
        path.push(lt);
    }
    path.push(";");
    path.push(Path::new(&system_root).join("System32"));
    if session {
        grants.push((root.to_path_buf(), Access::ReadWrite));
    } else {
        grants.push((root.to_path_buf(), Access::Read));
        grants.push((root.join("outputs"), Access::ReadWrite));
        grants.push((tmp.clone(), Access::ReadWrite));
    }
    let mut env: Vec<(String, OsString)> = vec![
        ("SystemRoot".into(), system_root),
        ("PATH".into(), path),
        ("TEMP".into(), tmp.clone().into_os_string()),
        ("TMP".into(), tmp.into_os_string()),
        ("NVIDIA_TF32_OVERRIDE".into(), "0".into()),
        ("CUDA_DEVICE_ORDER".into(), "PCI_BUS_ID".into()),
    ];
    // Windows rewrites LOCALAPPDATA to the AppContainer's own folder when it creates the process,
    // and refuses to create it (ERROR_ENVVAR_NOT_FOUND) when the block lacks it.
    if let Some(v) = std::env::var_os("LOCALAPPDATA") {
        env.push(("LOCALAPPDATA".into(), v));
    }
    if let Some(cache) = kernel_cache {
        grants.push((cache.to_path_buf(), Access::ReadWrite));
        // The driver's own caches, kept with the compiled kernels (see `sandbox::plan_with`).
        let driver = cache.join("driver").into_os_string();
        env.push(("__GL_SHADER_DISK_CACHE_PATH".into(), driver.clone()));
        env.push(("MESA_SHADER_CACHE_DIR".into(), driver));
        env.push(("__GL_SHADER_DISK_CACHE".into(), "1".into()));
        env.push(("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".into(), "1".into()));
        env.push((
            "CUDA_CACHE_PATH".into(),
            cache.join("cuda").into_os_string(),
        ));
    }
    Ok(Spec {
        program: exe.to_path_buf(),
        args,
        cwd: root.to_path_buf(),
        env,
        grants,
        memory,
        cpu_percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        let a: Vec<OsString> = args.iter().map(OsString::from).collect();
        let w = command_line(Path::new(r"C:\Program Files\x.exe"), &a);
        String::from_utf16_lossy(&w[..w.len() - 1])
    }

    #[test]
    fn arguments_are_quoted_as_windows_splits_them() {
        assert_eq!(
            line(&["run", "--task-dir", r"C:\a b\c\", "x\"y", ""]),
            r#""C:\Program Files\x.exe" run --task-dir "C:\a b\c\\" "x\"y" """#
        );
    }

    #[test]
    fn the_environment_block_is_sorted_and_double_terminated() {
        let env = environment(&[
            ("TEMP".into(), "t".into()),
            ("PATH".into(), "p".into()),
            ("SystemRoot".into(), r"C:\Windows".into()),
        ]);
        let s = String::from_utf16_lossy(&env);
        assert_eq!(s, "PATH=p\0SystemRoot=C:\\Windows\0TEMP=t\0\0");
    }
}
