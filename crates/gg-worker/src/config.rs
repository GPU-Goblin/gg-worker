//! Worker configuration (TOML + `GG_*` overrides). Local policy always wins over the server.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Agent configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Control plane base URL (`GG_SERVER_URL`).
    pub server_url: String,
    /// Ed25519 key file (`GG_WORKER_KEY`), created on first run.
    pub key_path: PathBuf,
    /// Working directory for task bundles.
    pub work_dir: PathBuf,
    /// Display name sent at registration.
    pub display_name: String,
    /// Enrollment code of a contributor account (`gge_…`, `GG_ENROLLMENT_TOKEN`): on its first start,
    /// a machine that is not yet authorized attaches itself to that account (ADR-0030). Empty:
    /// wait for an operator to authorize it.
    pub enrollment_token: String,
    /// Engine: `cpu`, `gpu` or `gpu:<index>` (`GG_WORKER_ENGINE`). GPU never falls back to CPU.
    pub engine: String,
    /// Compute kinds this worker accepts.
    pub kinds: Vec<String>,
    /// Local control API for the owner's application (ADR-0043), on this machine only; empty: none.
    pub control_listen: String,
    /// The owner's policy (pause, idle, hours, GPU); default: `policy.toml` next to the key. The
    /// control token (`control.token`, readable by the user alone) is written next to it.
    pub policy_path: Option<PathBuf>,
    /// Concurrent tasks (P1: 1).
    pub concurrency: u32,
    /// Local RAM limit for the executor (bytes).
    pub max_ram_bytes: u64,
    /// Accelerator memory the executor may use, bytes (`GG_WORKER_DEVICE_MEMORY`). Announced at
    /// every claim so that the server only offers tasks that fit (gg-server/docs/ROADMAP.md, C1), and enforced
    /// by the executor. For the CPU engine it defaults to `max_ram_bytes`; a GPU engine must
    /// declare it, with a margin below the card's memory, or it is offered no `graph.grad.v1` task.
    /// A value below the hardware simulates a smaller card.
    pub device_memory_bytes: Option<u64>,
    /// Local disk limit for bundles (bytes).
    pub max_disk_bytes: u64,
    /// Maximum size of a single downloaded artifact (bytes).
    pub max_download_bytes: u64,
    /// Local artifact cache budget in `work_dir/cache` (bytes, 0 disables). Entries are re-hashed
    /// on every hit; the cache is never a source of authority.
    pub cache_max_bytes: u64,
    /// Budget of the compiled GPU kernels kept between tasks in `work_dir/kernel-cache` (bytes, 0
    /// disables; ADR-0032). The only thing a task leaves to the next ones; emptied at start-up when
    /// over budget, and at every update of the executor.
    pub kernel_cache_max_bytes: u64,
    /// Directory of the LibTorch 2.9 libraries (`libtorch*.so`, `libc10*.so`…) of an executor built
    /// with the native CUDA engine (`engine = "cuda"`, ADR-0033); bound read-only in the sandbox.
    pub libtorch_dir: Option<PathBuf>,
    /// Keep one executor alive across the tasks of a training run on a GPU engine (ADR-0034):
    /// no GPU start-up per task. Off: a fresh executor per task, as on the CPU.
    pub persistent_executor: bool,
    /// Tasks after which an executor session is replaced by a fresh one.
    pub executor_session_max_tasks: u32,
    /// Long-poll wait when claiming.
    pub claim_wait_secs: u64,
    /// Heartbeat interval while executing.
    pub heartbeat_secs: u64,
    /// Path of the executor binary (default: `gg-executor` next to the agent).
    pub executor_path: Option<PathBuf>,
    /// Keep bundles of failed attempts for inspection.
    pub keep_failed_bundles: bool,
    /// Sandbox backend: `bwrap` (default: bubblewrap namespaces + Landlock/seccomp/rlimits in the
    /// executor) or `none` (development only, requires `dev_unsafe_no_isolation`).
    pub sandbox: String,
    /// cgroup limits through `systemd-run --user --scope`: `auto` (use when available), `require`
    /// (refuse to start without), `off`.
    pub cgroups: String,
    /// CPU quota for the executor in percent of one core (0 = no quota).
    pub cpu_quota_percent: u32,
    /// Explicit acknowledgement that the executor runs without OS-level isolation. Public
    /// deployments must refuse to start with this flag (see `worker doctor`).
    pub dev_unsafe_no_isolation: bool,
    /// Public mode: refuses to run without mandatory protections.
    pub public: bool,
    /// Controlled fault injection (development only, refused in public mode).
    pub dev_faults: DevFaults,
    /// Peer-assisted artifact distribution (ADR-0012). Off by default: nothing is fetched from
    /// peers and nothing is shared until the contributor turns it on.
    pub p2p: P2pConfig,
}

/// Peer-assisted distribution settings. Outbound sharing is explicit, bounded and switchable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct P2pConfig {
    /// Fetch artifacts from peers (chunks verified against the server-signed manifest) before
    /// falling back to the control plane.
    pub enabled: bool,
    /// Serve chunks of cached artifacts to readers presenting a valid server ticket.
    pub share: bool,
    /// Address the chunk server binds (port 0 picks a free port). `[::]:0` listens on IPv6 and, on
    /// a usual Linux, on IPv4 too; it falls back to IPv4 when the machine has no IPv6.
    pub listen_addr: String,
    /// Address announced to the control plane; empty means "use the bound address".
    pub advertise_addr: String,
    /// Concurrent uploads served.
    pub max_upload_conns: u32,
    /// Total bytes this process will serve to peers before refusing (0 disables sharing).
    pub upload_budget_bytes: u64,
    /// Peers tried per artifact.
    pub max_peers: u32,
    /// Contact loopback/private peer addresses (single-host demos, laboratory networks).
    pub allow_private_peers: bool,
    /// Per-chunk timeout when talking to a peer.
    pub request_timeout_secs: u64,
    /// Chunks requested at once when fetching an artifact (1 to 16).
    pub parallel_chunks: usize,
    /// Artifacts below this size are fetched from the control plane directly, not announced and
    /// not declared early: for a few kilobytes, the peer ceremony (announce, manifest, peer list,
    /// TLS handshake) costs more than it saves. Measured: 41 ms against 5 ms per staged task.
    pub min_bytes: u64,
    /// Milliseconds to wait for a chunk from the reader responsible for it (swarm) before asking
    /// the control plane anyway.
    pub swarm_patience_ms: u64,
    /// The durable copy of an activation declared before it (ADR-0026) leaves at once, but pauses
    /// while this worker serves chunks to peers (the next stage, on the critical path), for at most
    /// this many seconds in total.
    pub durable_copy_yield_secs: u64,
    /// Declare the activations of a staged task and submit before their durable copy (ADR-0026).
    /// Off: they are uploaded before submitting, as before (kept for measurements).
    pub early_handoff: bool,
    /// Announce a global IPv6 address when one exists. IPv6 needs no NAT traversal at all, so it is
    /// preferred over any port mapping; the control plane's dial-back still decides whether the
    /// address is really reachable (a box may block incoming IPv6).
    pub ipv6: bool,
    /// Ask the local gateway for a port mapping (NAT-PMP, RFC 6886) before announcing. When the
    /// gateway does not answer, the worker falls back to the address the control plane observed.
    pub nat_pmp: bool,
    /// Ask the local gateway for a port mapping over UPnP-IGD when NAT-PMP does not answer. Many
    /// home routers (measured on a consumer ISP router) speak only this one.
    pub upnp: bool,
    /// Lifetime requested for a port mapping, in seconds (both mechanisms). The mapping is renewed
    /// halfway through and deleted when the worker stops sharing.
    pub nat_pmp_lifetime_secs: u32,
    /// Gateway address override (`ip:port`); empty means the default IPv4 gateway on port 5351.
    pub nat_pmp_gateway: String,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            share: true,
            listen_addr: "[::]:0".into(),
            advertise_addr: String::new(),
            max_upload_conns: 4,
            upload_budget_bytes: 8 << 30,
            max_peers: 8,
            allow_private_peers: false,
            request_timeout_secs: 20,
            parallel_chunks: 4,
            min_bytes: 1 << 20,
            swarm_patience_ms: 2000,
            durable_copy_yield_secs: 20,
            early_handoff: true,
            ipv6: true,
            nat_pmp: true,
            upnp: true,
            nat_pmp_lifetime_secs: 3600,
            nat_pmp_gateway: String::new(),
        }
    }
}

/// Local fault injection for the reference demonstration and tests. Never remotely activatable.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DevFaults {
    /// Re-send every accepted submission once (must be answered as an idempotent replay).
    pub replay_submission: bool,
    /// Corrupt one byte of every output before upload (must be rejected by the validator).
    pub corrupt_outputs: bool,
    /// Hold each attempt this long after execution, heartbeating, before uploading (lets a demo
    /// interrupt the process while it holds a lease).
    pub hold_before_submit_secs: u64,
    /// Kill the executor once per attempt after its first checkpoint (exercises local resume).
    pub crash_executor_once: bool,
    /// Serve wrong bytes to peers asking for chunks (must be caught by the signed manifest).
    pub serve_corrupt_chunks: bool,
    /// Simulated uplink (bytes per second), shared by uploads to the control plane and chunks
    /// served to peers: measures what the order of transfers changes on a home link (ADR-0026).
    /// `0` leaves the real link alone.
    pub uplink_bytes_per_sec: u64,
}

impl DevFaults {
    /// Any fault enabled.
    pub fn any(&self) -> bool {
        self.replay_submission
            || self.corrupt_outputs
            || self.hold_before_submit_secs > 0
            || self.crash_executor_once
            || self.serve_corrupt_chunks
            || self.uplink_bytes_per_sec > 0
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:8080".into(),
            key_path: PathBuf::from("./var/worker/worker.key"),
            work_dir: PathBuf::from("./var/worker/work"),
            display_name: "worker".into(),
            enrollment_token: String::new(),
            engine: "cpu".into(),
            kinds: vec!["tensor.matmul.v1".into()],
            control_listen: "127.0.0.1:7460".into(),
            policy_path: None,
            concurrency: 1,
            max_ram_bytes: 4 << 30,
            device_memory_bytes: None,
            max_disk_bytes: 20 << 30,
            max_download_bytes: 2 << 30,
            cache_max_bytes: 4 << 30,
            kernel_cache_max_bytes: 1 << 30,
            libtorch_dir: None,
            persistent_executor: true,
            executor_session_max_tasks: 200,
            claim_wait_secs: 20,
            heartbeat_secs: 30,
            executor_path: None,
            keep_failed_bundles: false,
            sandbox: if cfg!(windows) {
                "appcontainer"
            } else {
                "bwrap"
            }
            .into(),
            cgroups: "auto".into(),
            cpu_quota_percent: 0,
            dev_unsafe_no_isolation: false,
            public: false,
            dev_faults: DevFaults::default(),
            p2p: P2pConfig::default(),
        }
    }
}

impl WorkerConfig {
    /// Where the owner's policy lives.
    pub fn policy_file(&self) -> PathBuf {
        self.policy_path.clone().unwrap_or_else(|| {
            self.key_path
                .parent()
                .map_or_else(|| PathBuf::from("policy.toml"), |d| d.join("policy.toml"))
        })
    }

    /// Where the control API's token lives.
    pub fn control_token_file(&self) -> PathBuf {
        self.policy_file().with_file_name("control.token")
    }
}

impl WorkerConfig {
    /// Accelerator memory announced and enforced: declared, or `max_ram_bytes` for the CPU engine,
    /// or `None` for an undeclared GPU (which then receives no task that needs accelerator memory).
    pub fn device_memory(&self) -> Option<u64> {
        match self.device_memory_bytes {
            Some(b) => Some(b),
            None if self.engine == "cpu" => Some(self.max_ram_bytes),
            None => None,
        }
    }

    /// Load from an optional TOML file with environment overrides.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut cfg = match path {
            Some(p) => toml::from_str::<WorkerConfig>(&std::fs::read_to_string(p)?)?,
            None => WorkerConfig::default(),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("GG_SERVER_URL") {
            cfg.server_url = v;
        }
        if let Some(v) = env("GG_WORKER_KEY") {
            cfg.key_path = PathBuf::from(v);
        }
        if let Some(v) = env("GG_WORKER_ENGINE") {
            cfg.engine = v;
        }
        if let Some(v) = env("GG_WORKER_WORK_DIR") {
            cfg.work_dir = PathBuf::from(v);
        }
        if let Some(v) = env("GG_ENROLLMENT_TOKEN") {
            cfg.enrollment_token = v;
        }
        if let Some(v) = env("GG_WORKER_NAME") {
            cfg.display_name = v;
        }
        if let Some(v) = env("GG_WORKER_CLAIM_WAIT_SECS").and_then(|v| v.parse().ok()) {
            cfg.claim_wait_secs = v;
        }
        if let Some(v) = env("GG_WORKER_DEVICE_MEMORY").and_then(|v| v.parse().ok()) {
            cfg.device_memory_bytes = Some(v);
        }
        if let Some(v) = env("GG_WORKER_SANDBOX") {
            cfg.sandbox = v;
        }
        if let Some(v) = env("GG_WORKER_P2P") {
            cfg.p2p.enabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// The configured kinds whose results this worker's engine may produce
    /// (`profiles::admitted_for`), and those it leaves out. A CUDA worker runs model tasks only
    /// (ADR-0033); a Vulkan worker's MLP results are not admitted (ADR-0031). Claiming a kind the
    /// engine cannot run burnt the task's attempts in a second (found by the beta rehearsal).
    pub fn claimable_kinds(&self) -> anyhow::Result<(Vec<String>, Vec<String>)> {
        let engine =
            gg_compute::EngineSelection::parse(&self.engine).map_err(|e| anyhow::anyhow!("{e}"))?;
        let profile = engine.numeric_profile();
        // A comparison of two computations (ADR-0042) runs on the CPU whatever the engine.
        Ok(self.kinds.iter().cloned().partition(|k| {
            gg_compute::profiles::admitted_for(k, profile) || k == "replica.compare.v1"
        }))
    }

    /// Validate local policy.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.concurrency == 1,
            "concurrency must be 1 in this version"
        );
        anyhow::ensure!(
            self.claim_wait_secs <= 120,
            "claim_wait_secs must be <= 120"
        );
        anyhow::ensure!(
            self.device_memory_bytes != Some(0),
            "device_memory_bytes must be positive when set"
        );
        anyhow::ensure!(!self.kinds.is_empty(), "kinds must not be empty");
        gg_compute::EngineSelection::parse(&self.engine).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (claimable, _) = self.claimable_kinds()?;
        anyhow::ensure!(
            !claimable.is_empty(),
            "none of the kinds {:?} can be run by engine '{}' (the native CUDA engine runs model \
             tasks only: graph.grad.v1, graph.stage.v1)",
            self.kinds,
            self.engine
        );
        if self.public && self.dev_unsafe_no_isolation {
            anyhow::bail!("dev_unsafe_no_isolation is incompatible with public mode");
        }
        anyhow::ensure!(
            matches!(self.cgroups.as_str(), "auto" | "require" | "off"),
            "cgroups must be auto, require or off"
        );
        let mode = crate::sandbox::SandboxMode::parse(&self.sandbox).ok_or_else(|| {
            anyhow::anyhow!(
                "sandbox must be bwrap, appcontainer or none, got '{}'",
                self.sandbox
            )
        })?;
        anyhow::ensure!(
            mode.available(),
            "sandbox = \"{}\" does not exist on {} (bwrap: Linux, appcontainer: Windows)",
            self.sandbox,
            std::env::consts::OS
        );
        if mode == crate::sandbox::SandboxMode::None && !self.dev_unsafe_no_isolation {
            anyhow::bail!("sandbox = \"none\" requires dev_unsafe_no_isolation = true");
        }
        if self.public && mode == crate::sandbox::SandboxMode::None {
            anyhow::bail!("sandbox = \"none\" is incompatible with public mode");
        }
        if self.public && self.dev_faults.any() {
            anyhow::bail!("dev_faults are incompatible with public mode");
        }
        gg_client::client::check_transport(&self.server_url, !self.public)
            .map_err(|e| anyhow::anyhow!("server_url: {e}"))?;
        if self.p2p.enabled {
            self.p2p
                .listen_addr
                .parse::<std::net::SocketAddr>()
                .map_err(|e| anyhow::anyhow!("p2p.listen_addr must be ip:port ({e})"))?;
            if !self.p2p.advertise_addr.is_empty() {
                self.p2p
                    .advertise_addr
                    .parse::<std::net::SocketAddr>()
                    .map_err(|e| anyhow::anyhow!("p2p.advertise_addr must be ip:port ({e})"))?;
            }
            anyhow::ensure!(
                (1..=64).contains(&self.p2p.max_peers),
                "p2p.max_peers must be between 1 and 64"
            );
            anyhow::ensure!(
                self.p2p.max_upload_conns >= 1,
                "p2p.max_upload_conns must be >= 1"
            );
            anyhow::ensure!(
                self.p2p.request_timeout_secs >= 1 && self.p2p.request_timeout_secs <= 300,
                "p2p.request_timeout_secs must be between 1 and 300"
            );
            if self.public && self.p2p.allow_private_peers {
                anyhow::bail!(
                    "p2p.allow_private_peers is incompatible with public mode (a tracker could point this worker at private addresses)"
                );
            }
            if self.cache_max_bytes == 0 && self.p2p.share {
                anyhow::bail!("p2p.share needs the artifact cache (cache_max_bytes > 0)");
            }
            if !self.p2p.nat_pmp_gateway.is_empty() {
                self.p2p
                    .nat_pmp_gateway
                    .parse::<std::net::SocketAddr>()
                    .map_err(|e| anyhow::anyhow!("p2p.nat_pmp_gateway must be ip:port ({e})"))?;
            }
            anyhow::ensure!(
                (60..=86_400).contains(&self.p2p.nat_pmp_lifetime_secs),
                "p2p.nat_pmp_lifetime_secs must be between 60 and 86400"
            );
        }
        Ok(())
    }

    /// Sandbox mode.
    pub fn sandbox_mode(&self) -> crate::sandbox::SandboxMode {
        crate::sandbox::SandboxMode::parse(&self.sandbox)
            .unwrap_or(crate::sandbox::SandboxMode::Bwrap)
    }

    /// Resolved executor path.
    pub fn executor(&self) -> anyhow::Result<PathBuf> {
        if let Some(p) = &self.executor_path {
            return Ok(p.clone());
        }
        let exe = std::env::current_exe()?;
        let dir = exe
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no parent dir for current exe"))?;
        Ok(dir.join(format!("gg-executor{}", std::env::consts::EXE_SUFFIX)))
    }
}

#[cfg(test)]
mod tests {
    use super::WorkerConfig;

    /// A worker claims only the kinds its engine may produce: the beta rehearsal saw a CUDA worker
    /// take a matrix product, fail it three times in a second, and fail the researcher's task.
    #[test]
    fn a_worker_claims_only_what_its_engine_runs() {
        let template: Vec<String> = [
            "tensor.matmul.v1",
            "mlp.train.v1",
            "mlp.grad.v1",
            "graph.grad.v1",
            "graph.stage.v1",
        ]
        .map(String::from)
        .into();
        let with = |engine: &str, kinds: &[String]| WorkerConfig {
            engine: engine.into(),
            kinds: kinds.to_vec(),
            ..WorkerConfig::default()
        };
        let (claimed, left_out) = with("cpu", &template).claimable_kinds().unwrap();
        assert_eq!((claimed, left_out.len()), (template.clone(), 0));
        let (claimed, left_out) = with("cuda", &template).claimable_kinds().unwrap();
        assert_eq!(claimed, ["graph.grad.v1", "graph.stage.v1"]);
        assert_eq!(
            left_out,
            ["tensor.matmul.v1", "mlp.train.v1", "mlp.grad.v1"]
        );
        let (claimed, _) = with("gpu", &template).claimable_kinds().unwrap();
        assert!(claimed.contains(&"tensor.matmul.v1".to_string()));
        assert!(claimed.contains(&"graph.grad.v1".to_string()));
        // A CUDA worker told to run matrix products only has nothing to claim: it refuses to start.
        let e = with("cuda:0", &["tensor.matmul.v1".to_string()])
            .validate()
            .unwrap_err()
            .to_string();
        assert!(e.contains("model tasks only"), "{e}");
    }

    #[test]
    fn a_public_worker_reaches_its_server_only_over_https() {
        let public = |url: &str| {
            let cfg = WorkerConfig {
                public: true,
                server_url: url.into(),
                ..WorkerConfig::default()
            };
            cfg.validate().map_err(|e| e.to_string())
        };
        let e = public("http://grid.example.org").unwrap_err();
        assert!(e.contains("server_url") && e.contains("use https"), "{e}");
        for ok in ["https://grid.example.org", "http://127.0.0.1:8080"] {
            if let Err(e) = public(ok) {
                assert!(!e.contains("server_url"), "{ok}: {e}");
            }
        }
        // Outside public mode, a development grid may use plain http on a private network.
        let dev = WorkerConfig {
            server_url: "http://192.168.1.10:8080".into(),
            ..WorkerConfig::default()
        };
        if let Err(e) = dev.validate() {
            assert!(!e.to_string().contains("server_url"), "{e}");
        }
    }
}
