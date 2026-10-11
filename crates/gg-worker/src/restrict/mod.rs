//! Executor self-restriction (applied inside the sandbox before any input is read):
//! Landlock filesystem rules, a seccomp deny-list and resource limits. Also the sandbox probe used
//! by `gg-worker doctor` to measure what is really enforced.

use std::path::Path;

use serde::{Deserialize, Serialize};

// The Linux layers (Landlock, seccomp, rlimits) and their probe. On Windows the executor is
// confined from the outside when it is launched (ADR-0045) and checks that confinement itself;
// elsewhere `apply_layers` refuses and the executor does not run restricted tasks.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{apply_layers, probe};

/// What was enforced.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Applied {
    /// Landlock ruleset status: `fully_enforced`, `enforced_abi_v3` or `enforced_abi_v4` (every
    /// right that kernel knows: [`landlock_holds`]), `partially_enforced`, `not_enforced`,
    /// `error: …`.
    pub landlock: String,
    /// `no_new_privs` set.
    pub no_new_privs: bool,
    /// seccomp filter installed.
    pub seccomp: bool,
    /// rlimits applied (`RLIMIT_FSIZE`, `RLIMIT_NOFILE`, optionally `RLIMIT_AS`).
    pub rlimits: Vec<String>,
}

/// Whether a Landlock status of [`Applied::landlock`] confines the executor as ADR-0009 requires:
/// fully enforced, or enforced with every right of an ABI v3 or v4 kernel (Linux 6.2 to 6.9). Those
/// know every filesystem right the ruleset asks for but `IoctlDev`, which the ruleset grants anyway
/// on `/dev`, the only directory of the sandbox holding device nodes.
pub fn landlock_holds(state: &str) -> bool {
    matches!(
        state,
        "fully_enforced" | "enforced_abi_v3" | "enforced_abi_v4"
    )
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
    "CUDA_DEVICE_ORDER",
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

/// The Linux restriction layers do not exist on this system: refuse, so that no task runs
/// believing it is restricted.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn apply_layers(_params: &RestrictParams<'_>, _layers: Layers) -> Result<Applied, String> {
    Err(format!(
        "executor self-restriction is not available on {}",
        std::env::consts::OS
    ))
}

/// The sandbox probe of this system: without restriction layers, only the GPU checks run and the
/// sandbox check fails.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn probe(
    bundle: &Path,
    _canary_port: u16,
    _canary_file: &Path,
    layers: Option<Layers>,
    gpu_init: Option<usize>,
    cuda_init: Option<usize>,
    _memory_limit: Option<u64>,
) -> ProbeReport {
    let apply_error = layers.map(|l| {
        apply_layers(
            &RestrictParams {
                bundle,
                max_address_space: None,
                max_file_bytes: 64 << 20,
                kernel_cache: None,
                session: false,
            },
            l,
        )
        .err()
        .unwrap_or_default()
    });
    let mut checks = Vec::new();
    if let Some(device_index) = gpu_init {
        checks.push(gpu_check(device_index));
    }
    if let Some(device_index) = cuda_init {
        checks.push(cuda_check(device_index));
    }
    checks.push(check(
        "sandbox_enforced",
        false,
        apply_error
            .clone()
            .unwrap_or_else(|| "restrictions not applied".into()),
    ));
    ProbeReport {
        applied: None,
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
