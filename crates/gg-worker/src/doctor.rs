//! `worker doctor`: reports which protections are really active. Nothing here is a guarantee
//! against a compromised driver; see gg-server/docs/THREAT_MODEL.md.

use std::path::Path;

use serde::Serialize;

use crate::config::WorkerConfig;

/// Status of a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Fine.
    Ok,
    /// Works but with a caveat.
    Warn,
    /// Blocking problem.
    Fail,
    /// Protection not implemented in this version.
    NotImplemented,
}

/// One check.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    /// Name.
    pub name: &'static str,
    /// Status.
    pub status: Status,
    /// Detail.
    pub detail: String,
}

/// Report.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Checks.
    pub checks: Vec<Check>,
    /// Whether the agent may start under the current configuration.
    pub may_start: bool,
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

#[cfg(not(windows))]
fn uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// Run all checks.
pub fn run(cfg: &WorkerConfig) -> Report {
    let mut checks = Vec::new();
    #[cfg(windows)]
    match crate::winsys::elevated() {
        Some(true) => checks.push(check(
            "not_root",
            Status::Fail,
            "running elevated (an administrator's full token) is refused: start it as the user, \
             from a standard account or with User Account Control on (with it off, an \
             administrator is always elevated)",
        )),
        Some(false) => checks.push(check("not_root", Status::Ok, "not elevated")),
        None => checks.push(check("not_root", Status::Warn, "cannot read the token")),
    }
    #[cfg(not(windows))]
    match uid() {
        Some(0) => checks.push(check(
            "not_root",
            Status::Fail,
            "running as root is refused",
        )),
        Some(u) => checks.push(check("not_root", Status::Ok, format!("uid {u}"))),
        None => checks.push(check(
            "not_root",
            Status::Warn,
            "cannot read /proc/self/status",
        )),
    }
    match cfg.executor() {
        Ok(p) if p.is_file() => checks.push(check(
            "executor_binary",
            Status::Ok,
            p.display().to_string(),
        )),
        Ok(p) => checks.push(check(
            "executor_binary",
            Status::Fail,
            format!("{} not found", p.display()),
        )),
        Err(e) => checks.push(check("executor_binary", Status::Fail, e.to_string())),
    }
    // C1: what the worker announces with every claim.
    checks.push(match cfg.device_memory() {
        Some(b) => check(
            "device_memory",
            Status::Ok,
            format!(
                "{b} bytes announced, {}; profile {}",
                if cfg.device_memory_bytes.is_some() {
                    "declared"
                } else {
                    "host memory of the CPU engine"
                },
                gg_compute::graph::PROFILE_VERSION
            ),
        ),
        None => check(
            "device_memory",
            Status::Warn,
            "GPU engine without device_memory_bytes: no graph.grad.v1 task will be offered; \
declare the card's memory, with a margin",
        ),
    });
    match std::fs::create_dir_all(&cfg.work_dir).and_then(|_| tempfile_probe(&cfg.work_dir)) {
        Ok(()) => checks.push(check(
            "work_dir_writable",
            Status::Ok,
            cfg.work_dir.display().to_string(),
        )),
        Err(e) => checks.push(check("work_dir_writable", Status::Fail, e.to_string())),
    }
    if cfg.key_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg.key_path)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0);
            if mode & 0o077 != 0 {
                checks.push(check(
                    "key_file_permissions",
                    Status::Fail,
                    format!("mode {mode:o} exposes the key"),
                ));
            } else {
                checks.push(check(
                    "key_file_permissions",
                    Status::Ok,
                    format!("mode {mode:o}"),
                ));
            }
        }
    } else {
        checks.push(check(
            "key_file_permissions",
            Status::Warn,
            "key file not created yet (run `gg-worker init`)",
        ));
    }
    let engine = gg_compute::EngineSelection::parse(&cfg.engine);
    match engine {
        Ok(gg_compute::EngineSelection::FlexCpu) => {
            checks.push(check("engine", Status::Ok, "cpu (burn flex)"))
        }
        Ok(gg_compute::EngineSelection::WgpuVulkan { device_index }) => {
            match probe_gpu(cfg, device_index) {
                Ok(info) => checks.push(check(
                    "engine",
                    Status::Ok,
                    format!("gpu:{device_index} {info}"),
                )),
                Err(e) => checks.push(check(
                    "engine",
                    Status::Fail,
                    format!("gpu:{device_index} unavailable: {e}"),
                )),
            }
        }
        Ok(gg_compute::EngineSelection::TorchCuda { device_index }) => {
            match probe_cuda(cfg, device_index) {
                Ok(info) => checks.push(check(
                    "engine",
                    Status::Ok,
                    format!("cuda:{device_index} {info}"),
                )),
                Err(e) => checks.push(check(
                    "engine",
                    Status::Fail,
                    format!("cuda:{device_index} unavailable: {e}"),
                )),
            }
        }
        Err(e) => checks.push(check("engine", Status::Fail, e.to_string())),
    }
    match cfg.claimable_kinds() {
        Ok((claimed, left_out)) if claimed.is_empty() => checks.push(check(
            "kinds",
            Status::Fail,
            format!("engine '{}' runs none of {left_out:?}", cfg.engine),
        )),
        Ok((claimed, left_out)) if left_out.is_empty() => {
            checks.push(check("kinds", Status::Ok, format!("{claimed:?}")))
        }
        Ok((claimed, left_out)) => checks.push(check(
            "kinds",
            Status::Ok,
            format!("{claimed:?}; not claimed with this engine: {left_out:?}"),
        )),
        Err(e) => checks.push(check("kinds", Status::Fail, e.to_string())),
    }
    // Isolation: measured by running the real sandbox with a breakout probe.
    let mandatory_missing = match cfg.sandbox_mode() {
        crate::sandbox::SandboxMode::AppContainer => match run_probe_windows(cfg) {
            // The Job Object bounds memory, processes and CPU: no cgroups to check.
            Ok(report) => {
                let mut missing = false;
                for c in &report.checks {
                    let status = if c.ok { Status::Ok } else { Status::Fail };
                    missing |= !c.ok;
                    checks.push(check(probe_name(&c.name), status, c.detail.clone()));
                }
                if let Some(e) = &report.apply_error {
                    checks.push(check("sandbox_restrict", Status::Fail, e.clone()));
                    missing = true;
                }
                missing
            }
            Err(e) => {
                checks.push(check("sandbox", Status::Fail, format!("probe failed: {e}")));
                true
            }
        },
        crate::sandbox::SandboxMode::None => {
            checks.push(check(
                "sandbox",
                Status::NotImplemented,
                "sandbox = \"none\": executor runs unconfined (development only)",
            ));
            true
        }
        crate::sandbox::SandboxMode::Bwrap => match crate::sandbox::find_bwrap() {
            None => {
                checks.push(check(
                    "sandbox",
                    Status::Fail,
                    "bubblewrap (bwrap) not found in PATH",
                ));
                true
            }
            Some(bwrap) => match run_probe(cfg, &bwrap) {
                Ok(report) => {
                    let mut missing = false;
                    for c in &report.checks {
                        let status = if c.ok { Status::Ok } else { Status::Fail };
                        missing |= !c.ok;
                        checks.push(check(probe_name(&c.name), status, c.detail.clone()));
                    }
                    if let Some(e) = &report.apply_error {
                        checks.push(check("sandbox_restrict", Status::Fail, e.clone()));
                        missing = true;
                    } else if let Some(a) = &report.applied {
                        checks.push(check("sandbox_rlimits", Status::Ok, a.rlimits.join(", ")));
                    }
                    match cfg.cgroups.as_str() {
                        "off" => checks.push(check(
                            "cgroup_limits",
                            Status::Warn,
                            "cgroups = \"off\": memory/cpu bounded by rlimits only",
                        )),
                        mode => match probe_cgroups(cfg) {
                            Ok(detail) => checks.push(check("cgroup_limits", Status::Ok, detail)),
                            Err(e) if mode == "require" => {
                                checks.push(check(
                                    "cgroup_limits",
                                    Status::Fail,
                                    format!("required but unavailable: {e}"),
                                ));
                                missing = true;
                            }
                            Err(e) => checks.push(check(
                                "cgroup_limits",
                                Status::Warn,
                                format!("unavailable, falling back to rlimits: {e}"),
                            )),
                        },
                    }
                    missing
                }
                Err(e) => {
                    checks.push(check("sandbox", Status::Fail, format!("probe failed: {e}")));
                    true
                }
            },
        },
    };
    let has_fail = checks.iter().any(|c| c.status == Status::Fail);
    let may_start = if cfg.public && mandatory_missing {
        checks.push(check(
            "public_mode",
            Status::Fail,
            "public mode requires every sandbox protection to hold",
        ));
        false
    } else if cfg.public {
        !has_fail
    } else if mandatory_missing && !cfg.dev_unsafe_no_isolation {
        checks.push(check("dev_mode_ack", Status::Fail, "set dev_unsafe_no_isolation = true to run without OS-level isolation (development only)"));
        false
    } else {
        if mandatory_missing {
            checks.push(check(
                "dev_mode_ack",
                Status::Warn,
                "running WITHOUT OS-level isolation (dev_unsafe_no_isolation = true)",
            ));
        }
        !has_fail
    };
    Report { checks, may_start }
}

fn tempfile_probe(dir: &Path) -> std::io::Result<()> {
    let p = dir.join(".doctor-probe");
    std::fs::write(&p, b"ok")?;
    std::fs::remove_file(p)
}

/// Probe the GPU in a subprocess (a missing adapter panics inside the runtime; a child process
/// contains that).
/// Run `gg-executor probe-cuda` with the LibTorch libraries and strict float32, as a task would.
pub fn probe_cuda(cfg: &WorkerConfig, device_index: usize) -> Result<String, String> {
    let exe = cfg.executor().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("probe-cuda")
        .arg("--device")
        .arg(device_index.to_string())
        .env_clear()
        .env("NVIDIA_TF32_OVERRIDE", "0");
    windows_base_env(&mut cmd);
    if let Some(dir) = &cfg.libtorch_dir {
        if cfg!(windows) {
            // LibTorch's DLLs first, then the system's.
            let mut path = dir.clone().into_os_string();
            if let Some(system) = std::env::var_os("PATH") {
                path.push(";");
                path.push(system);
            }
            cmd.env("PATH", path);
        } else {
            cmd.env("LD_LIBRARY_PATH", dir);
        }
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(probe_error(&out))
    }
}

pub fn probe_gpu(cfg: &WorkerConfig, device_index: usize) -> Result<String, String> {
    let exe = cfg.executor().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("probe-gpu")
        .arg("--device")
        .arg(device_index.to_string())
        .env_clear();
    windows_base_env(&mut cmd);
    let out = cmd.output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(probe_error(&out))
    }
}

/// Why a probe failed: the last line it wrote, or what its exit code means.
fn probe_error(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    match err.lines().rfind(|l| !l.trim().is_empty()) {
        Some(line) => line.chars().take(200).collect(),
        None => match out.status.code() {
            Some(code) => exit_meaning(code as u32),
            None => format!("probe failed ({})", out.status),
        },
    }
}

/// What a Windows exit code says when the program could not even start.
fn exit_meaning(code: u32) -> String {
    match code {
        0xC000_0135 => "a library the program needs was not found (STATUS_DLL_NOT_FOUND): for the \
                        CUDA engine, Microsoft's Visual C++ runtime or LibTorch's libraries"
            .into(),
        0xC000_0142 => "a library failed to initialize (STATUS_DLL_INIT_FAILED)".into(),
        0xC000_007B => {
            "a library is of the wrong architecture (STATUS_INVALID_IMAGE_FORMAT)".into()
        }
        _ => format!("probe failed (exit code {code:#x})"),
    }
}

/// Windows programs need `SystemRoot` (and the system's `PATH`) to load their system libraries,
/// even with an otherwise empty environment.
fn windows_base_env(cmd: &mut std::process::Command) {
    if cfg!(windows) {
        for var in ["SystemRoot", "PATH"] {
            if let Some(v) = std::env::var_os(var) {
                cmd.env(var, v);
            }
        }
    }
}

fn probe_name(name: &str) -> &'static str {
    match name {
        "pid_namespace" => "sandbox_pid_namespace",
        "network_isolated" => "sandbox_network_isolated",
        "network_interfaces" => "sandbox_network_interfaces",
        "seccomp_active" => "sandbox_seccomp",
        "mount_isolated" => "sandbox_mount_isolated",
        "system_read_only" => "sandbox_system_read_only",
        "bundle_root_read_only" => "sandbox_bundle_read_only",
        "outputs_writable" => "sandbox_outputs_writable",
        "exec_denied" => "sandbox_exec_denied",
        "environment_clean" => "sandbox_environment_clean",
        "not_root" => "sandbox_not_root",
        "landlock_enforced" => "sandbox_landlock",
        "gpu_init_restricted" => "sandbox_gpu_init",
        "internet_isolated" => "sandbox_internet_isolated",
        "profile_unreadable" => "sandbox_profile_unreadable",
        "child_process_denied" => "sandbox_child_process",
        "memory_limited" => "sandbox_memory_limited",
        "appcontainer_token" => "sandbox_appcontainer",
        "job_limits" => "sandbox_job_limits",
        "sandbox_enforced" => "sandbox_enforced",
        "cuda_init_restricted" => "sandbox_cuda_init",
        _ => "sandbox_other",
    }
}

/// Whether cgroup limits can be applied on this host (used by the agent for each task).
pub fn cgroups_available(cfg: &WorkerConfig) -> bool {
    cfg.cgroups != "off" && probe_cgroups(cfg).is_ok()
}

/// Launch `gg-executor probe-cgroup` through `systemd-run --user --scope` with a 256 MiB limit and
/// check that the limit is visible from inside.
pub fn probe_cgroups(cfg: &WorkerConfig) -> Result<String, String> {
    let systemd_run = crate::sandbox::find_systemd_run().ok_or("systemd-run not found in PATH")?;
    let exe = cfg.executor().map_err(|e| e.to_string())?;
    let limits = crate::sandbox::CgroupLimits {
        memory_max: 256 << 20,
        cpu_percent: Some(50),
        tasks_max: 64,
    };
    let (program, args) = crate::sandbox::wrap_with_cgroup(
        &systemd_run,
        limits,
        &exe,
        &[std::ffi::OsString::from("probe-cgroup")],
    );
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).env_clear();
    for k in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    // This probe runs the executor outside the sandbox, where nothing binds LibTorch at
    // /opt/libtorch: a CUDA executor would not even load without its libraries (found by the
    // install rehearsal of ADR-0039). The same libraries the sandbox binds for tasks.
    if let Some(dir) = &cfg.libtorch_dir {
        cmd.env("LD_LIBRARY_PATH", dir);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "systemd-run failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    let report: crate::restrict::CgroupReport =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("probe output: {e}"))?;
    if report.memory_max != (256u64 << 20).to_string() {
        return Err(format!(
            "memory.max is '{}' instead of the requested limit",
            report.memory_max
        ));
    }
    if !report.cpu_max.starts_with("50000 ") {
        return Err(format!(
            "cpu.max is '{}' instead of the requested quota",
            report.cpu_max
        ));
    }
    Ok(format!(
        "systemd-run user scope enforces memory.max/cpu.max/pids.max (probe: {} / {} / {})",
        report.memory_max, report.cpu_max, report.pids_max
    ))
}

/// Run the breakout probe inside the real sandbox configuration (same plan as tasks).
pub fn run_probe(cfg: &WorkerConfig, bwrap: &Path) -> Result<crate::restrict::ProbeReport, String> {
    let exe = cfg.executor().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&cfg.work_dir).map_err(|e| e.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("doctor-")
        .tempdir_in(&cfg.work_dir)
        .map_err(|e| e.to_string())?;
    let bundle = crate::bundle::Bundle {
        root: scratch.path().to_path_buf(),
    };
    bundle.create().map_err(|e| e.to_string())?;
    let canary_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let canary_file = canary_dir.path().join("canary");
    std::fs::write(&canary_file, b"host secret").map_err(|e| e.to_string())?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let engine = gg_compute::EngineSelection::parse(&cfg.engine).map_err(|e| e.to_string())?;
    let port_s = port.to_string();
    let canary_s = canary_file.to_string_lossy().into_owned();
    let mut inner: Vec<&str> = vec![
        "probe",
        "--task-dir",
        crate::bundle::SANDBOX_BUNDLE_DIR,
        "--canary-port",
        &port_s,
        "--canary-file",
        &canary_s,
    ];
    let gpu_index;
    match engine {
        gg_compute::EngineSelection::WgpuVulkan { device_index } => {
            gpu_index = device_index.to_string();
            inner.push("--gpu-init");
            inner.push(&gpu_index);
        }
        gg_compute::EngineSelection::TorchCuda { device_index } => {
            gpu_index = device_index.to_string();
            inner.push("--cuda-init");
            inner.push(&gpu_index);
        }
        gg_compute::EngineSelection::FlexCpu => {}
    }
    // The probe runs the executor as a task would: an executor built with the native CUDA engine
    // links LibTorch, and does not even start without it.
    let plan = crate::sandbox::plan_with(
        bwrap,
        &exe,
        &bundle.root,
        engine,
        &inner,
        None,
        cfg.libtorch_dir.as_deref(),
    )?;
    let out = crate::sandbox::std_command(&plan)
        .output()
        .map_err(|e| e.to_string())?;
    drop(listener);
    if !out.status.success() {
        return Err(format!(
            "probe exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("probe output: {e}"))
}

/// Run the breakout probe inside the Windows sandbox, launched exactly as a task is (ADR-0045).
#[cfg(windows)]
pub fn run_probe_windows(cfg: &WorkerConfig) -> Result<crate::restrict::ProbeReport, String> {
    let exe = cfg.executor().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&cfg.work_dir).map_err(|e| e.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("doctor-")
        .tempdir_in(&cfg.work_dir)
        .map_err(|e| e.to_string())?;
    let bundle = crate::bundle::Bundle {
        root: scratch.path().to_path_buf(),
    };
    bundle.create().map_err(|e| e.to_string())?;
    // In the user's own temporary directory, which the sandbox is not granted.
    let canary_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let canary_file = canary_dir.path().join("canary");
    std::fs::write(&canary_file, b"host secret").map_err(|e| e.to_string())?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let engine = gg_compute::EngineSelection::parse(&cfg.engine).map_err(|e| e.to_string())?;
    let mut args: Vec<std::ffi::OsString> = vec![
        "probe".into(),
        "--task-dir".into(),
        bundle.root.clone().into(),
        "--canary-port".into(),
        port.to_string().into(),
        "--canary-file".into(),
        canary_file.clone().into(),
        "--memory-limit".into(),
        cfg.max_ram_bytes
            .saturating_add(512 << 20)
            .to_string()
            .into(),
    ];
    match engine {
        gg_compute::EngineSelection::WgpuVulkan { device_index } => {
            args.extend(["--gpu-init".into(), device_index.to_string().into()]);
        }
        gg_compute::EngineSelection::TorchCuda { device_index } => {
            args.extend(["--cuda-init".into(), device_index.to_string().into()]);
        }
        gg_compute::EngineSelection::FlexCpu => {}
    }
    let spec = crate::appcontainer::executor_spec(
        &exe,
        &bundle.root,
        false,
        args,
        None,
        cfg.libtorch_dir.as_deref(),
        cfg.max_ram_bytes.saturating_add(512 << 20),
        (cfg.cpu_quota_percent > 0).then_some(cfg.cpu_quota_percent),
    )?;
    // Its own runtime on its own thread: doctor may be called from inside another runtime.
    let out = std::thread::spawn(move || -> Result<(u32, Vec<u8>, Vec<u8>), String> {
        use tokio::io::AsyncReadExt;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let mut child = crate::appcontainer::spawn(&spec, false).map_err(|e| e.to_string())?;
            let (mut o, mut e) = (Vec::new(), Vec::new());
            if let Some(s) = child.stdout.as_mut() {
                let _ = s.read_to_end(&mut o).await;
            }
            if let Some(s) = child.stderr.as_mut() {
                let _ = s.take(64 << 10).read_to_end(&mut e).await;
            }
            let code = child.wait().await.map_err(|e| e.to_string())?;
            Ok((code, o, e))
        })
    })
    .join()
    .map_err(|_| "the probe thread panicked".to_owned())??;
    drop(listener);
    let (code, stdout, stderr) = out;
    if code != 0 {
        let err = String::from_utf8_lossy(&stderr);
        return Err(if err.trim().is_empty() {
            exit_meaning(code)
        } else {
            format!(
                "probe exited with code {code:#x}: {}",
                err.chars().take(300).collect::<String>()
            )
        });
    }
    serde_json::from_slice(&stdout).map_err(|e| format!("probe output: {e}"))
}

/// The Windows sandbox exists only on Windows.
#[cfg(not(windows))]
pub fn run_probe_windows(_cfg: &WorkerConfig) -> Result<crate::restrict::ProbeReport, String> {
    Err("the appcontainer sandbox exists only on Windows".into())
}

/// Human-readable rendering.
pub fn render(report: &Report) -> String {
    let mut s = String::new();
    for c in &report.checks {
        let tag = match c.status {
            Status::Ok => "OK  ",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::NotImplemented => "N/I ",
        };
        s.push_str(&format!("[{tag}] {:<24} {}\n", c.name, c.detail));
    }
    s.push_str(if report.may_start {
        "result: agent may start\n"
    } else {
        "result: agent refuses to start\n"
    });
    s
}
