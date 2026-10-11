//! `gg-executor`: limited child process. It receives a bundle directory and an engine name, never
//! network credentials, and writes bounded outputs plus a report. With `--restrict` it applies
//! Landlock, seccomp and rlimits to itself before reading any input (it is normally launched
//! inside bubblewrap by the agent, see `gg_worker::sandbox`).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gg_compute::EngineSelection;
use gg_protocol::messages::TaskSpecV1;
use gg_worker::bundle::{Bundle, ExecutorReport, ReportOutput};
use gg_worker::restrict::{self, RestrictParams};

#[derive(Parser)]
#[command(
    name = "gg-executor",
    about = "GPU Goblin executor (child process of gg-worker)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Execute the bundle in `task_dir`.
    Run {
        /// Bundle directory.
        #[arg(long)]
        task_dir: PathBuf,
        /// Engine: cpu | gpu | gpu:<index>.
        #[arg(long, default_value = "cpu")]
        engine: String,
        /// Apply Landlock, seccomp and rlimits before reading inputs.
        #[arg(long)]
        restrict: bool,
        /// Accelerator memory this executor may use, bytes (C1). A task whose estimate exceeds it
        /// is refused before any computation.
        #[arg(long)]
        device_memory: Option<u64>,
        /// Directory of compiled GPU kernels kept between tasks (ADR-0032), read-write.
        #[arg(long)]
        kernel_cache: Option<PathBuf>,
    },
    /// Serve tasks from the attempt bundles under `root`, one per line of standard input
    /// (executor session, ADR-0034).
    Serve {
        /// Directory of the session's attempt bundles.
        #[arg(long)]
        root: PathBuf,
        /// Engine: cpu | gpu | gpu:<index> | cuda | cuda:<index>.
        #[arg(long, default_value = "cpu")]
        engine: String,
        /// Apply Landlock, seccomp and rlimits before reading any task.
        #[arg(long)]
        restrict: bool,
        /// Accelerator memory this executor may use, bytes.
        #[arg(long)]
        device_memory: Option<u64>,
        /// Directory of compiled GPU kernels kept between tasks (ADR-0032).
        #[arg(long)]
        kernel_cache: Option<PathBuf>,
        /// Largest file a task may write (every task's output budget is also enforced).
        #[arg(long, default_value_t = 16 << 30)]
        max_file_bytes: u64,
    },
    /// Check the native CUDA engine on this device and print its description (exit 1 if unavailable).
    ProbeCuda {
        /// CUDA device index.
        #[arg(long, default_value_t = 0)]
        device: usize,
    },
    /// Initialize the Vulkan device and print its description (exit 1 if unavailable).
    ProbeGpu {
        /// Discrete GPU index.
        #[arg(long, default_value_t = 0)]
        device: usize,
    },
    /// Print the cgroup v2 limits of the current process as JSON (used by `gg-worker doctor`).
    ProbeCgroup,
    /// Apply restrictions, then try to escape the sandbox; print a JSON report (used by `gg-worker doctor`).
    Probe {
        /// Bundle directory (a scratch bundle created by the agent).
        #[arg(long)]
        task_dir: PathBuf,
        /// TCP port the agent listens on outside the sandbox.
        #[arg(long)]
        canary_port: u16,
        /// Host file outside the sandbox mounts.
        #[arg(long)]
        canary_file: PathBuf,
        /// Skip self-restriction (measures the namespace layer alone).
        #[arg(long)]
        no_restrict: bool,
        /// Diagnostic: skip one restriction layer (`rlimits`, `landlock`, `seccomp`); repeatable.
        #[arg(long = "skip-layer")]
        skip_layers: Vec<String>,
        /// Also initialize the Vulkan device at this index under restriction.
        #[arg(long)]
        gpu_init: Option<usize>,
        /// Also check the native CUDA engine on this device under restriction.
        #[arg(long)]
        cuda_init: Option<usize>,
        /// The sandbox's memory limit, in bytes: allocating past it must fail (Windows).
        #[arg(long)]
        memory_limit: Option<u64>,
    },
}

fn restrict_self(
    root: &std::path::Path,
    engine: EngineSelection,
    max_address_space: Option<u64>,
    max_file_bytes: u64,
    kernel_cache: Option<&std::path::Path>,
    session: bool,
) -> anyhow::Result<()> {
    let params = RestrictParams {
        bundle: root,
        // RLIMIT_AS breaks GPU drivers (large virtual reservations); CPU runs get the task limit.
        max_address_space: match engine {
            EngineSelection::FlexCpu => max_address_space,
            EngineSelection::WgpuVulkan { .. } | EngineSelection::TorchCuda { .. } => None,
        },
        max_file_bytes,
        kernel_cache,
        session,
    };
    let applied = restrict::apply_all(&params)
        .map_err(|e| anyhow::anyhow!("self-restriction failed: {e}"))?;
    // On Windows `apply_all` fails unless the whole sandbox holds (ADR-0045).
    if cfg!(target_os = "linux") && !restrict::landlock_holds(&applied.landlock) {
        anyhow::bail!(
            "Landlock is {} on this kernel; refusing to run",
            applied.landlock
        );
    }
    eprintln!(
        "restricted: landlock={} seccomp={} rlimits={:?}",
        applied.landlock, applied.seccomp, applied.rlimits
    );
    Ok(())
}

fn set_kernel_cache(
    engine: EngineSelection,
    kernel_cache: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    #[cfg(feature = "gpu")]
    if let (Some(dir), EngineSelection::WgpuVulkan { .. }) = (kernel_cache, engine) {
        gg_compute::engine::gpu::set_kernel_cache(dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let _ = (engine, kernel_cache);
    Ok(())
}

/// Execute one bundle and write its report; returns the number of outputs and the profile.
fn execute_bundle(
    bundle: &Bundle,
    spec: &TaskSpecV1,
    engine: EngineSelection,
    device_memory: Option<u64>,
) -> anyhow::Result<(usize, &'static str)> {
    let task = gg_compute::validate_definition(&spec.definition)?;
    let inputs = bundle.inputs(spec.definition.inputs.keys().cloned());
    let report = gg_compute::execute_task_limited(
        engine,
        &task,
        &inputs,
        &bundle.outputs_dir(),
        device_memory,
    )?;
    let out = ExecutorReport {
        outputs: report
            .outputs
            .iter()
            .map(|(n, f)| ReportOutput {
                name: n.clone(),
                file_name: f
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                hash: f.hash,
                size: f.size,
            })
            .collect(),
        execution_profile: report.engine.to_profile(report.duration_ms),
        metrics: report.metrics,
    };
    let tmp = bundle.report_file().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&out)?)?;
    std::fs::rename(&tmp, bundle.report_file())?;
    Ok((out.outputs.len(), engine.numeric_profile()))
}

fn read_spec(bundle: &Bundle) -> anyhow::Result<TaskSpecV1> {
    let spec_bytes = std::fs::read(bundle.task_file())?;
    Ok(gg_protocol::canonical::from_bytes(&spec_bytes)?)
}

fn run(
    task_dir: PathBuf,
    engine: &str,
    restrict_self_flag: bool,
    device_memory: Option<u64>,
    kernel_cache: Option<PathBuf>,
) -> anyhow::Result<()> {
    let engine = EngineSelection::parse(engine).map_err(|e| anyhow::anyhow!("{e}"))?;
    let bundle = Bundle { root: task_dir };
    // Limits are read from the spec header before restricting; the spec itself is small.
    let spec = read_spec(&bundle)?;
    if restrict_self_flag {
        let limits = &spec.definition.resource_limits;
        restrict_self(
            &bundle.root,
            engine,
            Some(limits.max_ram_bytes.max(1 << 30)),
            limits.max_output_bytes.max(1 << 20).saturating_mul(2),
            kernel_cache.as_deref(),
            false,
        )?;
    }
    set_kernel_cache(engine, kernel_cache.as_deref())?;
    let (n, profile) = execute_bundle(&bundle, &spec, engine, device_memory)?;
    println!("ok: {n} output(s) on {profile}");
    Ok(())
}

/// One request of an executor session: the name of an attempt directory under the session root.
#[derive(serde::Deserialize)]
struct SessionRequest {
    task: String,
}

/// An executor session (ADR-0034): restricted once, then one task per line of standard input,
/// one JSON answer per line of standard output, until standard input closes. GPU state (context,
/// compiled kernels, cached parameters) survives from one task to the next; the worker scopes a
/// session to one training run and recycles it.
fn serve(
    root: PathBuf,
    engine: &str,
    restrict_self_flag: bool,
    device_memory: Option<u64>,
    kernel_cache: Option<PathBuf>,
    max_file_bytes: u64,
) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    let engine = EngineSelection::parse(engine).map_err(|e| anyhow::anyhow!("{e}"))?;
    if restrict_self_flag {
        restrict_self(
            &root,
            engine,
            None,
            max_file_bytes,
            kernel_cache.as_deref(),
            true,
        )?;
    }
    set_kernel_cache(engine, kernel_cache.as_deref())?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let answer = match serde_json::from_str::<SessionRequest>(&line) {
            Ok(req)
                if !req.task.is_empty()
                    && req.task.len() <= 128
                    && req
                        .task
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
            {
                let bundle = Bundle {
                    root: root.join(&req.task),
                };
                match read_spec(&bundle)
                    .and_then(|spec| execute_bundle(&bundle, &spec, engine, device_memory))
                {
                    Ok((n, profile)) => {
                        serde_json::json!({ "ok": true, "outputs": n, "profile": profile })
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
                }
            }
            _ => serde_json::json!({ "ok": false, "error": "malformed request" }),
        };
        writeln!(stdout, "{answer}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn probe_cuda(device: usize) -> anyhow::Result<()> {
    #[cfg(feature = "cuda")]
    {
        let info = gg_compute::engine::cuda::check(device).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{}", info.device_name);
        Ok(())
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = device;
        anyhow::bail!("this executor was built without the native CUDA engine (feature `cuda`)")
    }
}

fn probe_gpu(device: usize) -> anyhow::Result<()> {
    #[cfg(feature = "gpu")]
    {
        let (_dev, info) =
            gg_compute::engine::gpu::init_vulkan(device).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{}", info.device_name);
        Ok(())
    }
    #[cfg(not(feature = "gpu"))]
    {
        let _ = device;
        anyhow::bail!("this executor was built without GPU support (feature `gpu`)")
    }
}

fn main() {
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Run {
            task_dir,
            engine,
            restrict,
            device_memory,
            kernel_cache,
        } => run(task_dir, &engine, restrict, device_memory, kernel_cache),
        Cmd::ProbeGpu { device } => probe_gpu(device),
        Cmd::ProbeCuda { device } => probe_cuda(device),
        Cmd::Serve {
            root,
            engine,
            restrict,
            device_memory,
            kernel_cache,
            max_file_bytes,
        } => serve(
            root,
            &engine,
            restrict,
            device_memory,
            kernel_cache,
            max_file_bytes,
        ),
        Cmd::ProbeCgroup => {
            println!(
                "{}",
                serde_json::to_string(&gg_worker::restrict::cgroup_limits()).unwrap_or_default()
            );
            Ok(())
        }
        Cmd::Probe {
            task_dir,
            canary_port,
            canary_file,
            no_restrict,
            skip_layers,
            gpu_init,
            cuda_init,
            memory_limit,
        } => {
            let layers = if no_restrict {
                None
            } else {
                Some(restrict::Layers {
                    rlimits: !skip_layers.iter().any(|l| l == "rlimits"),
                    landlock: !skip_layers.iter().any(|l| l == "landlock"),
                    seccomp: !skip_layers.iter().any(|l| l == "seccomp"),
                })
            };
            let report = restrict::probe(
                &task_dir,
                canary_port,
                &canary_file,
                layers,
                gpu_init,
                cuda_init,
                memory_limit,
            );
            println!("{}", serde_json::to_string(&report).unwrap_or_default());
            Ok(())
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
