//! Agent-side sandbox: launches the executor inside bubblewrap with user/mount/pid/net/ipc/uts
//! namespaces, no network, a read-only minimal system view, a cleaned environment and only the
//! bundle writable. The executor then restricts itself further (Landlock, seccomp, rlimits; see
//! [`crate::restrict`]). None of this protects against a compromised kernel driver.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gg_compute::EngineSelection;

use crate::bundle::{
    SANDBOX_BUNDLE_DIR, SANDBOX_EXECUTOR_PATH, SANDBOX_KERNEL_CACHE_DIR, SANDBOX_LIBTORCH_DIR,
};

/// Sandbox backend selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxMode {
    /// bubblewrap namespaces + executor self-restriction (Linux).
    Bwrap,
    /// AppContainer + Job Object + executor self-check (Windows, ADR-0045).
    AppContainer,
    /// No OS-level isolation (development only, requires explicit acknowledgement).
    None,
}

impl SandboxMode {
    /// Parse `bwrap` | `appcontainer` | `none`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bwrap" => Some(Self::Bwrap),
            "appcontainer" => Some(Self::AppContainer),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Whether this sandbox exists on this system.
    pub fn available(self) -> bool {
        match self {
            Self::Bwrap => cfg!(target_os = "linux"),
            Self::AppContainer => cfg!(windows),
            Self::None => true,
        }
    }
}

/// Read-only system directories exposed to the executor (only those that exist on the host).
pub const RO_SYSTEM_PATHS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/etc/ld.so.cache",
    "/etc/alternatives",
];
/// Additional read-only paths needed by the Vulkan loader.
pub const RO_GPU_PATHS: &[&str] = &["/etc/vulkan", "/usr/share/vulkan"];
/// Device nodes required by the NVIDIA Vulkan driver (index-independent).
pub const GPU_DEVICE_NODES: &[&str] = &["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/dri"];
/// Device nodes required by CUDA (index-independent): no display nodes.
pub const CUDA_DEVICE_NODES: &[&str] = &["/dev/nvidiactl", "/dev/nvidia-uvm"];

/// cgroup v2 limits applied through a transient `systemd-run --user --scope` (delegated user slice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CgroupLimits {
    /// `MemoryMax` in bytes.
    pub memory_max: u64,
    /// `CPUQuota` in percent of one CPU (100 = one full core), `None` for no CPU quota.
    pub cpu_percent: Option<u32>,
    /// `TasksMax` (threads + processes).
    pub tasks_max: u32,
}

/// Locate `systemd-run` (used for cgroup limits when the user slice delegates controllers).
pub fn find_systemd_run() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("systemd-run"))
            .find(|c| c.is_file())
    })
}

/// Wrap an argument vector with `systemd-run --user --scope` and the given limits.
pub fn wrap_with_cgroup(
    systemd_run: &Path,
    limits: CgroupLimits,
    program: &Path,
    args: &[OsString],
) -> (PathBuf, Vec<OsString>) {
    let mut out: Vec<OsString> = vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        "-p".into(),
        format!("MemoryMax={}", limits.memory_max).into(),
        "-p".into(),
        format!("TasksMax={}", limits.tasks_max).into(),
    ];
    if let Some(cpu) = limits.cpu_percent {
        out.push("-p".into());
        out.push(format!("CPUQuota={cpu}%").into());
    }
    out.push("--".into());
    out.push(program.as_os_str().to_owned());
    out.extend(args.iter().cloned());
    (systemd_run.to_path_buf(), out)
}

/// Locate bubblewrap.
pub fn find_bwrap() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("bwrap"))
            .find(|c| c.is_file())
    })
}

/// GPU device nodes to bind for a discrete GPU index (empty for CPU).
pub fn gpu_devices(engine: EngineSelection) -> Vec<PathBuf> {
    match engine {
        EngineSelection::FlexCpu => Vec::new(),
        EngineSelection::WgpuVulkan { device_index } => {
            let mut v: Vec<PathBuf> = GPU_DEVICE_NODES.iter().map(PathBuf::from).collect();
            v.push(PathBuf::from(format!("/dev/nvidia{device_index}")));
            v
        }
        EngineSelection::TorchCuda { device_index } => {
            let mut v: Vec<PathBuf> = CUDA_DEVICE_NODES.iter().map(PathBuf::from).collect();
            v.push(PathBuf::from(format!("/dev/nvidia{device_index}")));
            v
        }
    }
}

/// A fully specified sandbox launch.
#[derive(Clone, Debug)]
pub struct SandboxPlan {
    /// bubblewrap binary.
    pub bwrap: PathBuf,
    /// Arguments up to and including the inner program and its arguments.
    pub args: Vec<OsString>,
    /// Device nodes bound into the sandbox.
    pub devices: Vec<PathBuf>,
    /// Optional cgroup wrapper (`systemd-run`, limits).
    pub cgroup: Option<(PathBuf, CgroupLimits)>,
}

impl SandboxPlan {
    /// Add cgroup limits through `systemd-run`.
    pub fn with_cgroup(mut self, systemd_run: &Path, limits: CgroupLimits) -> Self {
        self.cgroup = Some((systemd_run.to_path_buf(), limits));
        self
    }

    /// Program and arguments actually executed.
    pub fn program_and_args(&self) -> (PathBuf, Vec<OsString>) {
        match &self.cgroup {
            Some((sr, limits)) => wrap_with_cgroup(sr, *limits, &self.bwrap, &self.args),
            None => (self.bwrap.clone(), self.args.clone()),
        }
    }
}

/// Build the bubblewrap invocation for `inner_args` (executor subcommand and options).
pub fn plan(
    bwrap: &Path,
    executor: &Path,
    bundle: &Path,
    engine: EngineSelection,
    inner_args: &[&str],
) -> Result<SandboxPlan, String> {
    plan_with(bwrap, executor, bundle, engine, inner_args, None, None)
}

/// Like [`plan`], also binding `kernel_cache` read-write at [`SANDBOX_KERNEL_CACHE_DIR`] for a GPU
/// engine (ADR-0032): the compiled kernels outlive the task, and only them. `libtorch`: the LibTorch
/// libraries of an executor built with the native CUDA engine (ADR-0033), read-only at
/// [`SANDBOX_LIBTORCH_DIR`], for every engine since the executor links them.
pub fn plan_with(
    bwrap: &Path,
    executor: &Path,
    bundle: &Path,
    engine: EngineSelection,
    inner_args: &[&str],
    kernel_cache: Option<&Path>,
    libtorch: Option<&Path>,
) -> Result<SandboxPlan, String> {
    let executor = executor
        .canonicalize()
        .map_err(|e| format!("executor path: {e}"))?;
    let bundle = bundle
        .canonicalize()
        .map_err(|e| format!("bundle path: {e}"))?;
    let mut args: Vec<OsString> = Vec::new();
    let push = |args: &mut Vec<OsString>, items: &[&str]| {
        for i in items {
            args.push(OsString::from(i));
        }
    };
    push(
        &mut args,
        &[
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ],
    );
    push(
        &mut args,
        &[
            "--setenv",
            "RUST_LOG",
            "info",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
        ],
    );
    for p in RO_SYSTEM_PATHS {
        if Path::new(p).exists() {
            push(&mut args, &["--ro-bind", p, p]);
        }
    }
    push(
        &mut args,
        &["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"],
    );
    let mut devices = Vec::new();
    if let Some(dir) = libtorch {
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("libtorch path: {e}"))?;
        args.push("--ro-bind".into());
        args.push(dir.into());
        args.push(SANDBOX_LIBTORCH_DIR.into());
        push(
            &mut args,
            &["--setenv", "LD_LIBRARY_PATH", SANDBOX_LIBTORCH_DIR],
        );
    }
    if matches!(engine, EngineSelection::TorchCuda { .. }) {
        // Strict float32: no TF32 in cuDNN or cuBLAS (ADR-0033).
        push(&mut args, &["--setenv", "NVIDIA_TF32_OVERRIDE", "0"]);
        // `cuda:<index>` numbers the cards as `nvidia-smi` does (the bus order), so that each worker
        // of a machine with several cards computes on the card whose temperature it watches; CUDA's
        // own default puts the fastest first.
        push(&mut args, &["--setenv", "CUDA_DEVICE_ORDER", "PCI_BUS_ID"]);
    }
    if matches!(
        engine,
        EngineSelection::WgpuVulkan { .. } | EngineSelection::TorchCuda { .. }
    ) {
        if matches!(engine, EngineSelection::WgpuVulkan { .. }) {
            for p in RO_GPU_PATHS {
                if Path::new(p).exists() {
                    push(&mut args, &["--ro-bind", p, p]);
                }
            }
        }
        for d in gpu_devices(engine) {
            if !d.exists() {
                return Err(format!("GPU device node {} is absent", d.display()));
            }
            args.push("--dev-bind".into());
            args.push(d.clone().into());
            args.push(d.clone().into());
            devices.push(d);
        }
    }
    args.push("--ro-bind".into());
    args.push(executor.into());
    args.push(SANDBOX_EXECUTOR_PATH.into());
    args.push("--bind".into());
    args.push(bundle.into());
    args.push(SANDBOX_BUNDLE_DIR.into());
    if let Some(cache) = kernel_cache
        && matches!(
            engine,
            EngineSelection::WgpuVulkan { .. } | EngineSelection::TorchCuda { .. }
        )
    {
        let cache = cache
            .canonicalize()
            .map_err(|e| format!("kernel cache path: {e}"))?;
        args.push("--bind".into());
        args.push(cache.into());
        args.push(SANDBOX_KERNEL_CACHE_DIR.into());
        // The driver turns each SPIR-V kernel into machine code and keeps it in its own cache,
        // under `HOME` by default, which is an empty tmpfs here: without these, the driver
        // compiled everything again for every task (4.7 of the 5.5 s of a ViT-tiny first call).
        // `driver/` is created by the worker (`kernel_cache::prepare`).
        let driver = format!("{SANDBOX_KERNEL_CACHE_DIR}/driver");
        for var in ["__GL_SHADER_DISK_CACHE_PATH", "MESA_SHADER_CACHE_DIR"] {
            push(&mut args, &["--setenv", var, &driver]);
        }
        // CUDA's own just-in-time cache (PTX compiled for this card).
        let cuda = format!("{SANDBOX_KERNEL_CACHE_DIR}/cuda");
        push(&mut args, &["--setenv", "CUDA_CACHE_PATH", &cuda]);
        push(
            &mut args,
            &[
                "--setenv",
                "__GL_SHADER_DISK_CACHE",
                "1",
                "--setenv",
                "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP",
                "1",
            ],
        );
    }
    push(&mut args, &["--chdir", "/", SANDBOX_EXECUTOR_PATH]);
    push(&mut args, inner_args);
    Ok(SandboxPlan {
        bwrap: bwrap.to_path_buf(),
        args,
        devices,
        cgroup: None,
    })
}

/// Build the (tokio) command for a plan.
pub fn command(plan: &SandboxPlan) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&plan.bwrap);
    cmd.args(&plan.args).env_clear();
    cmd
}

/// Build the (std) command for a plan.
pub fn std_command(plan: &SandboxPlan) -> std::process::Command {
    let mut cmd = std::process::Command::new(&plan.bwrap);
    cmd.args(&plan.args).env_clear();
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled-kernel cache (ADR-0032) is the only other writable bind, and only for a GPU
    /// engine: a CPU task compiles nothing and gets no cache. (bubblewrap: Linux only.)
    #[test]
    #[cfg(target_os = "linux")]
    fn the_kernel_cache_is_bound_for_a_gpu_engine_only() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"").unwrap();
        let bundle = dir.path().join("b");
        let cache = dir.path().join("kc");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let args = |engine| {
            plan_with(
                Path::new("/usr/bin/bwrap"),
                &exe,
                &bundle,
                engine,
                &["run"],
                Some(&cache),
                None,
            )
            .unwrap()
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };
        let gpu = args(EngineSelection::WgpuVulkan { device_index: 0 });
        let cpu = args(EngineSelection::FlexCpu);
        let bound = |a: &[String]| {
            a.windows(3).any(|w| {
                w[0] == "--bind"
                    && w[1] == cache.canonicalize().unwrap().to_string_lossy()
                    && w[2] == SANDBOX_KERNEL_CACHE_DIR
            })
        };
        assert!(bound(&gpu), "{gpu:?}");
        assert!(!bound(&cpu), "{cpu:?}");
        assert_eq!(gpu.iter().filter(|a| *a == "--bind").count(), 2);
    }

    /// The native CUDA engine (ADR-0033): LibTorch read-only, strict float32 imposed, the CUDA
    /// device nodes and no display node.
    #[test]
    fn a_cuda_plan_binds_libtorch_read_only_and_imposes_strict_float32() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"").unwrap();
        let bundle = dir.path().join("b");
        let torch = dir.path().join("libtorch");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::create_dir_all(&torch).unwrap();
        let plan = plan_with(
            Path::new("/usr/bin/bwrap"),
            &exe,
            &bundle,
            EngineSelection::TorchCuda { device_index: 0 },
            &["run"],
            None,
            Some(&torch),
        );
        // Device nodes may be absent on a machine without an NVIDIA driver: the plan says so.
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                assert!(e.contains("/dev/nvidia"), "{e}");
                return;
            }
        };
        let a: Vec<String> = plan
            .args
            .iter()
            .map(|x| x.to_string_lossy().into_owned())
            .collect();
        let has = |w: &[&str]| a.windows(w.len()).any(|x| x == w);
        let torch_s = torch.canonicalize().unwrap().to_string_lossy().into_owned();
        assert!(has(&["--ro-bind", &torch_s, SANDBOX_LIBTORCH_DIR]), "{a:?}");
        assert!(
            has(&["--setenv", "LD_LIBRARY_PATH", SANDBOX_LIBTORCH_DIR]),
            "{a:?}"
        );
        assert!(has(&["--setenv", "NVIDIA_TF32_OVERRIDE", "0"]), "{a:?}");
        assert!(
            has(&["--setenv", "CUDA_DEVICE_ORDER", "PCI_BUS_ID"]),
            "{a:?}"
        );
        assert!(
            plan.devices.iter().all(|d| !d.starts_with("/dev/dri")),
            "{:?}",
            plan.devices
        );
        assert!(plan.devices.contains(&PathBuf::from("/dev/nvidia-uvm")));
        // LibTorch is never writable.
        assert!(!has(&["--bind", &torch_s, SANDBOX_LIBTORCH_DIR]));
    }

    #[test]
    fn plan_mounts_only_bundle_writable_and_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"").unwrap();
        let bundle = dir.path().join("b");
        std::fs::create_dir_all(&bundle).unwrap();
        let p = plan(
            Path::new("/usr/bin/bwrap"),
            &exe,
            &bundle,
            EngineSelection::FlexCpu,
            &["run", "--task-dir", "/bundle"],
        )
        .unwrap();
        let s: Vec<String> = p
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(s.contains(&"--unshare-all".to_string()));
        assert!(s.contains(&"--clearenv".to_string()));
        assert!(!s.contains(&"--share-net".to_string()));
        let binds: Vec<&String> = s.iter().filter(|a| *a == "--bind").collect();
        assert_eq!(binds.len(), 1, "only the bundle is bound read-write");
        assert!(p.devices.is_empty());
        let g = plan(
            Path::new("/usr/bin/bwrap"),
            &exe,
            &bundle,
            EngineSelection::WgpuVulkan { device_index: 0 },
            &["run"],
        );
        match g {
            Ok(g) => assert!(g.devices.iter().any(|d| d.ends_with("nvidia0"))),
            Err(e) => assert!(e.contains("absent")),
        }
    }
}
