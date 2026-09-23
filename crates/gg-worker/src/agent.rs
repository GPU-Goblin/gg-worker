//! Agent loop: register, authenticate, claim, download, execute in a child process, upload, submit.

use std::process::Stdio;
use std::time::{Duration, Instant};

use gg_compute::EngineSelection;
use gg_core::*;
use gg_protocol::domain::{Domain, hash_message};
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::{ArtifactRef, AssignmentV1, ResultSubmissionV1, TaskSpecV1};
use gg_protocol::signed::Signed;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::bundle::{Bundle, ExecutorReport};
use crate::config::WorkerConfig;
use gg_client::client::{Api, ClientError, ServerInfo};

/// Claim response.
#[derive(Clone, Debug, Deserialize)]
pub struct ClaimResponse {
    /// Signed assignment.
    pub assignment: Signed<AssignmentV1>,
    /// Task spec.
    pub spec: TaskSpecV1,
    /// Inputs.
    pub inputs: Vec<NamedArtifact>,
}

/// Named artifact.
#[derive(Clone, Debug, Deserialize)]
pub struct NamedArtifact {
    /// Name.
    pub name: String,
    /// Hash.
    pub hash: Sha256Digest,
    /// Size.
    pub size: u64,
}

/// Outcome of one attempt from the agent's point of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Submitted successfully.
    Submitted {
        /// Attempt.
        attempt_id: AttemptId,
    },
    /// Abandoned with a reason.
    Abandoned {
        /// Attempt.
        attempt_id: AttemptId,
        /// Reason.
        reason: String,
    },
}

/// The agent.
/// An address this worker could be reached at, with how it was obtained and when it must be
/// resolved again. The control plane dials each candidate back before advertising it, so the worker
/// simply offers them in order of preference.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// `ip:port` (IPv6 in brackets).
    pub addr: String,
    /// How it was obtained: `configured`, `ipv6`, `nat-pmp`, `upnp`, `bound` or `observed`.
    pub how: &'static str,
    /// When this address must be resolved again.
    pub renew: Instant,
}

impl Candidate {
    fn new(addr: String, how: &'static str, valid_for: Duration) -> Self {
        Self {
            addr,
            how,
            renew: Instant::now() + valid_for,
        }
    }
}

pub struct Agent {
    cfg: WorkerConfig,
    /// The kinds claimed: those of the configuration this engine may produce.
    kinds: Vec<String>,
    api: Api,
    key: SecretKey,
    info: ServerInfo,
    worker_id: WorkerId,
    token: Option<(String, Instant)>,
    engine: EngineSelection,
    cgroups_available: bool,
    /// Compiled GPU kernels lent to every executor (ADR-0032); `None` for the CPU or when disabled.
    kernel_cache: Option<std::path::PathBuf>,
    /// Directory of the attempt bundles an executor session serves (ADR-0034); `None`: a fresh
    /// executor per task.
    session_root: Option<std::path::PathBuf>,
    /// The executor session, if one is running.
    session: tokio::sync::Mutex<Option<crate::session::Session>>,
    cache: gg_client::cache::ArtifactCache,
    peer_server: Option<crate::p2p::PeerServer>,
    /// Address announced to the control plane and when it must be resolved again (a NAT mapping
    /// expires; an observed address can change).
    peer_addr: tokio::sync::Mutex<Option<(String, Instant)>>,
    /// UPnP mapping this worker created, removed when it stops sharing.
    upnp_mapping: tokio::sync::Mutex<Option<(crate::upnp::Gateway, u16)>>,
    /// Attempts processed (for tests and reports).
    pub processed: Vec<AttemptOutcome>,
    /// Activations declared before their durable copy (ADR-0026), waiting for the submission to be
    /// recorded before they are uploaded in the background.
    deferred: Vec<DeferredUpload>,
}

/// What fetching one input took.
#[derive(Default)]
struct InputFetch {
    cache_hit: bool,
    peer_chunks: u32,
    http_chunks: u32,
    bad_chunks: u32,
}

/// How long a task waits for an input that is still travelling before its durable copy: past this,
/// neither its producer nor the server has it, and the attempt is abandoned.
const PENDING_INPUT_WAIT_SECS: u64 = 60;

/// An activation declared to the server and served by peers, whose durable copy is still to send.
struct DeferredUpload {
    attempt_id: AttemptId,
    hash: Sha256Digest,
    size: u64,
    path: std::path::PathBuf,
}

/// Keeps an attempt's lease alive for its whole life (inputs, execution, outputs, submission), not
/// only while the executor runs: a large input on a home connection can take longer to download
/// than a lease lasts. Stops when dropped or when the server says the lease is gone.
pub struct LeaseKeeper {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    lost: tokio::sync::watch::Receiver<Option<String>>,
}

impl LeaseKeeper {
    /// Heartbeat `attempt` every `every` until stopped.
    pub fn start(api: Api, token: String, attempt: AttemptId, every: Duration) -> Self {
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let (lost_tx, lost_rx) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = &mut stop_rx => return,
                }
                match api.heartbeat(&token, attempt).await {
                    Ok(_) => {}
                    Err(ClientError::Status {
                        status: 404 | 409,
                        message,
                    }) => {
                        let _ = lost_tx.send(Some(message));
                        return;
                    }
                    // A transient failure: the lease outlasts several missed heartbeats.
                    Err(e) => tracing::warn!(error = %e, "heartbeat failed, retrying"),
                }
            }
        });
        Self {
            stop: Some(stop_tx),
            lost: lost_rx,
        }
    }

    /// Why the lease was lost, if the server said so.
    pub fn lost(&self) -> Option<String> {
        self.lost.borrow().clone()
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Send the durable copies of declared activations, after the submission is recorded. The server
/// verifies nothing that depends on them before they arrive, and reissues the round if they never
/// do: failures are retried, then logged.
async fn upload_deferred(
    api: Api,
    token: String,
    list: Vec<DeferredUpload>,
    served: Option<std::sync::Arc<crate::p2p::ShareStats>>,
    yield_secs: u64,
) {
    for d in list {
        let path = format!("/worker/attempts/{}/outputs/{}", d.attempt_id, d.hash);
        let mut delay = std::time::Duration::from_millis(200);
        for attempt in 1..=6u32 {
            // The reader comes first: the durable-copy upload steps aside while this worker
            // serves chunks to a peer (the next stage, on the critical path), and resumes as soon
            // as the link is free.
            match api
                .upload_yielding(
                    &token,
                    &path,
                    &d.path,
                    &d.hash,
                    d.size,
                    "ggt1-f32",
                    served
                        .clone()
                        .map(|s| s as std::sync::Arc<dyn gg_client::client::YieldTo>),
                    yield_secs,
                )
                .await
            {
                Ok(_) => break,
                Err(e) if attempt < 6 => {
                    tracing::warn!(error = %e, attempt, hash = %d.hash, "durable copy of an activation failed, retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(10));
                }
                Err(e) => {
                    tracing::warn!(error = %e, hash = %d.hash, "durable copy of an activation abandoned: the round will be reissued")
                }
            }
        }
        let _ = tokio::fs::remove_file(&d.path).await;
    }
}

impl Agent {
    /// Load the key, discover the server and register.
    pub async fn new(cfg: WorkerConfig) -> anyhow::Result<Self> {
        cfg.validate()?;
        let key = SecretKey::load_or_generate(&cfg.key_path)?;
        let uplink = (cfg.dev_faults.uplink_bytes_per_sec > 0).then(|| {
            tracing::warn!(
                bytes_per_sec = cfg.dev_faults.uplink_bytes_per_sec,
                "DEV FAULT: simulated uplink"
            );
            std::sync::Arc::new(gg_client::throttle::Uplink::new(
                cfg.dev_faults.uplink_bytes_per_sec,
            ))
        });
        // Public mode: https only (plain http to this machine aside). Outside it, a development
        // grid may run over plain http on a private network.
        let api = if cfg.public {
            Api::new(&cfg.server_url)?
        } else {
            Api::new_allowing_plain_http(&cfg.server_url)?
        }
        .with_uplink(uplink.clone());
        let info = api.info().await?;
        anyhow::ensure!(
            info.protocol_version == gg_protocol::PROTOCOL_VERSION,
            "server protocol version {} unsupported",
            info.protocol_version
        );
        let reg = api
            .register_worker(&key.public_key(), &cfg.display_name)
            .await?;
        tracing::info!(worker_id = %reg.worker_id, authorized = reg.authorized, key = %key.public_key(), network = %info.network_id, "registered");
        let engine = EngineSelection::parse(&cfg.engine).map_err(|e| anyhow::anyhow!("{e}"))?;
        let cgroups_available = cfg.sandbox_mode() == crate::sandbox::SandboxMode::Bwrap
            && crate::doctor::cgroups_available(&cfg);
        if cfg.cgroups == "require" && !cgroups_available {
            anyhow::bail!("cgroups = \"require\" but systemd-run user scopes are unavailable");
        }
        tracing::info!(cgroups = cgroups_available, "executor resource limits");
        let kernel_cache = if matches!(
            engine,
            EngineSelection::WgpuVulkan { .. } | EngineSelection::TorchCuda { .. }
        ) && cfg.sandbox_mode() == crate::sandbox::SandboxMode::Bwrap
        {
            crate::kernel_cache::prepare(
                &cfg.work_dir,
                &cfg.executor()?,
                cfg.kernel_cache_max_bytes,
            )?
        } else {
            None
        };
        if let Some(dir) = &kernel_cache {
            tracing::info!(dir = %dir.display(), "compiled GPU kernels kept between tasks");
        }
        // Executor sessions: GPU engines only (a CPU task keeps its own process and its own
        // address-space limit). The directory holds attempt bundles and nothing else: it is what
        // the session's sandbox binds.
        let session_root = if cfg.persistent_executor
            && matches!(
                engine,
                EngineSelection::WgpuVulkan { .. } | EngineSelection::TorchCuda { .. }
            ) {
            let root = cfg.work_dir.join("session");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root)?;
            tracing::info!(dir = %root.display(), "one executor per training run (sessions)");
            Some(root)
        } else {
            None
        };
        let cache =
            gg_client::cache::ArtifactCache::new(cfg.work_dir.join("cache"), cfg.cache_max_bytes);
        // Sharing is explicit: nothing is served unless p2p.enabled and p2p.share are both on.
        let peer_server = if cfg.p2p.enabled && cfg.p2p.share {
            let server = crate::p2p::PeerServer::start_with_faults(
                &cfg.p2p.listen_addr,
                cache.clone(),
                info.network_id.clone(),
                info.server_public_key,
                cfg.p2p.max_upload_conns,
                cfg.p2p.upload_budget_bytes,
                cfg.dev_faults.serve_corrupt_chunks,
                uplink.clone(),
            )
            .await?;
            tracing::info!(bound = %server.addr(), "peer sharing enabled");
            Some(server)
        } else {
            None
        };
        let (kinds, left_out) = cfg.claimable_kinds()?;
        if !left_out.is_empty() {
            tracing::info!(engine = %cfg.engine, claimed = ?kinds, left_out = ?left_out, "kinds this engine does not run are not claimed");
        }
        Ok(Self {
            cfg,
            kinds,
            api,
            key,
            info,
            worker_id: reg.worker_id,
            token: None,
            engine,
            cgroups_available,
            kernel_cache,
            session_root,
            session: tokio::sync::Mutex::new(None),
            cache,
            peer_server,
            peer_addr: tokio::sync::Mutex::new(None),
            upnp_mapping: tokio::sync::Mutex::new(None),
            processed: Vec::new(),
            deferred: Vec::new(),
        })
    }

    /// Worker id.
    pub fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    /// Public key.
    pub fn public_key(&self) -> gg_protocol::keys::PublicKey {
        self.key.public_key()
    }

    async fn session(&mut self) -> Result<String, ClientError> {
        if let Some((t, exp)) = &self.token
            && Instant::now() + Duration::from_secs(60) < *exp
        {
            return Ok(t.clone());
        }
        let s = self
            .api
            .login(&self.key, &self.info.network_id, "worker-login")
            .await?;
        let exp = Instant::now() + Duration::from_secs(s.expires_in_secs.max(90) as u64);
        self.token = Some((s.token.clone(), exp));
        Ok(s.token)
    }

    /// Attach this machine to the contributor account of the configured enrollment code. The login
    /// proves the key; the server then authorizes the machine.
    async fn enroll(&mut self) -> Result<(), ClientError> {
        let token = self.session().await?;
        self.api
            .post_json::<serde_json::Value>(
                &token,
                "/worker/enroll",
                &serde_json::json!({ "enrollment_token": self.cfg.enrollment_token.trim() }),
            )
            .await?;
        // The session was opened before the machine was authorized: open a fresh one.
        self.token = None;
        Ok(())
    }

    /// Wait until an operator authorizes this key (polls registration), or attach it with the
    /// configured enrollment code.
    pub async fn wait_authorized(
        &mut self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<bool> {
        loop {
            let reg = self
                .api
                .register_worker(&self.key.public_key(), &self.cfg.display_name)
                .await?;
            if reg.authorized {
                return Ok(true);
            }
            if !self.cfg.enrollment_token.is_empty() {
                match self.enroll().await {
                    Ok(()) => {
                        tracing::info!(worker_id = %reg.worker_id, "attached to the contributor account of the enrollment code");
                        return Ok(true);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "enrollment refused: check the code on the contributor portal");
                        anyhow::bail!("enrollment refused: {e}");
                    }
                }
            }
            tracing::warn!(worker_id = %reg.worker_id, key = %self.key.public_key(), "waiting for operator authorization (gg admin worker-authorize <id>)");
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                _ = shutdown.changed() => { if *shutdown.borrow() { return Ok(false); } }
            }
        }
    }

    /// Claim and process tasks until shutdown. `max_tasks` bounds the run (None = unbounded).
    pub async fn run(
        &mut self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        max_tasks: Option<usize>,
    ) -> anyhow::Result<()> {
        if !self.wait_authorized(shutdown.clone()).await? {
            return Ok(());
        }
        let mut done = 0usize;
        loop {
            if *shutdown.borrow() || max_tasks.is_some_and(|m| done >= m) {
                self.stop_sharing().await;
                return Ok(());
            }
            let token = match self.session().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "login failed, retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            // Capabilities (C1): the server only offers what fits. An undeclared GPU announces 0
            // accelerator bytes, which excludes every task that needs some.
            let body = serde_json::json!({
                "kinds": self.kinds,
                "wait_secs": self.cfg.claim_wait_secs,
                "ram_bytes": self.cfg.max_ram_bytes,
                "device_memory_bytes": self.cfg.device_memory().unwrap_or(0),
                "profile": gg_compute::graph::PROFILE_VERSION,
                "engine": self.cfg.engine,
                "agent_version": env!("CARGO_PKG_VERSION"),
                // The server sizes the lease on this rhythm (several missed heartbeats), so that a
                // worker that vanishes is replaced quickly.
                "heartbeat_secs": self.cfg.heartbeat_secs,
            });
            let claim = tokio::select! {
                r = self.api.post_json::<ClaimResponse>(&token, "/worker/claim", &body) => r,
                _ = shutdown.changed() => { continue; }
            };
            match claim {
                Ok(Some(c)) => {
                    done += 1;
                    let outcome = self.process(&token, c, shutdown.clone()).await;
                    self.processed.push(outcome);
                }
                Ok(None) => {}
                Err(ClientError::Status { status: 401, .. }) => {
                    self.token = None;
                }
                Err(ClientError::Status {
                    status: 426,
                    message,
                }) => {
                    // Retrying cannot help: this binary is older than the server accepts.
                    self.stop_sharing().await;
                    tracing::error!(%message, "this worker must be updated");
                    anyhow::bail!("update required: {message}");
                }
                Err(ClientError::Status {
                    status: 403,
                    message,
                }) => {
                    tracing::warn!(%message, "not authorized; waiting");
                    self.token = None;
                    if !self.wait_authorized(shutdown.clone()).await? {
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "claim failed, backing off");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }

    async fn process(
        &mut self,
        token: &str,
        claim: ClaimResponse,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> AttemptOutcome {
        let attempt_id = claim.assignment.body.attempt_id;
        let span = tracing::info_span!("attempt", task_id = %claim.assignment.body.task_id, attempt_id = %attempt_id);
        let _g = span.enter();
        let keeper = LeaseKeeper::start(
            self.api.clone(),
            token.to_owned(),
            attempt_id,
            Duration::from_secs(self.cfg.heartbeat_secs.max(1)),
        );
        let mut outcome = self.prepare_and_execute(token, &claim, &mut shutdown).await;
        if let (Ok(_), Some(why)) = (&outcome, keeper.lost()) {
            outcome = Err(format!("lease lost before submission: {why}"));
        }
        self.unpin_inputs(&claim);
        match outcome {
            Ok(submission) => match self.submit_timed(token, &submission).await {
                Ok(v) => {
                    tracing::info!(recorded = %v["recorded"], "result submitted");
                    let deferred = std::mem::take(&mut self.deferred);
                    if !deferred.is_empty() {
                        tokio::spawn(upload_deferred(
                            self.api.clone(),
                            token.to_owned(),
                            deferred,
                            self.peer_server.as_ref().map(|p| p.stats()),
                            self.cfg.p2p.durable_copy_yield_secs,
                        ));
                    }
                    if self.cfg.dev_faults.replay_submission {
                        match self.api.submit_result(token, &submission).await {
                            Ok(r) => {
                                tracing::warn!(recorded = %r["recorded"], same_hash = %(r["submission_hash"] == v["submission_hash"]), "DEV FAULT: replayed submission")
                            }
                            Err(e) => tracing::warn!(error = %e, "DEV FAULT: replay refused"),
                        }
                    }
                    drop(keeper);
                    AttemptOutcome::Submitted { attempt_id }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "submission refused");
                    for d in std::mem::take(&mut self.deferred) {
                        let _ = std::fs::remove_file(&d.path);
                    }
                    AttemptOutcome::Abandoned {
                        attempt_id,
                        reason: format!("submission refused: {e}"),
                    }
                }
            },
            Err(reason) => {
                tracing::warn!(%reason, "attempt abandoned");
                if let Err(e) = self.api.abandon(token, attempt_id, &reason).await {
                    tracing::debug!(error = %e, "abandon not accepted (lease may already be gone)");
                }
                AttemptOutcome::Abandoned { attempt_id, reason }
            }
        }
    }

    /// Submit a result, logging how long it took.
    async fn submit_timed(
        &self,
        token: &str,
        submission: &Signed<ResultSubmissionV1>,
    ) -> Result<serde_json::Value, ClientError> {
        let t_submit = Instant::now();
        let r = self.api.submit_result(token, submission).await;
        tracing::info!(
            submit_ms = t_submit.elapsed().as_millis() as u64,
            "submission timed"
        );
        r
    }

    /// Fetch one input into `dest`: from the cache, from peers (large artifacts, or an
    /// activation still travelling before its durable copy), or from the control plane.
    async fn fetch_input(
        &self,
        token: &str,
        name: &str,
        r: &ArtifactRef,
        dest: &std::path::Path,
    ) -> Result<InputFetch, String> {
        let mut out = InputFetch::default();
        let large = r.size >= self.cfg.p2p.min_bytes;
        let sharing = self.cfg.p2p.enabled && self.peer_server.is_some();
        match self.cache.fetch(&r.hash, r.size, dest) {
            Ok(gg_client::cache::Lookup::Hit) => {
                out.cache_hit = true;
                if sharing && large {
                    self.announce(token, &r.hash).await;
                }
                return Ok(out);
            }
            Ok(gg_client::cache::Lookup::Corrupted) => {
                tracing::warn!(input = name, hash = %r.hash, "cached artifact corrupted, discarded")
            }
            Ok(gg_client::cache::Lookup::Miss) => {}
            Err(e) => tracing::warn!(input = name, error = %e, "artifact cache unusable"),
        }
        if r.size > self.cfg.max_download_bytes {
            return Err(format!(
                "input {name} is {} bytes, above the local limit",
                r.size
            ));
        }
        let http_path = format!("/worker/artifacts/{}", r.hash);
        // Small artifact: one request to the control plane costs less than the peer-to-peer
        // ceremony (announce, manifest, peer list, TLS handshake). Peers are used only if the
        // server does not have it yet (an activation handed off before its durable copy, 409).
        let mut use_peers = self.cfg.p2p.enabled && large;
        if !use_peers {
            match self
                .api
                .download_retry(token, &http_path, r, self.cfg.max_download_bytes, dest)
                .await
            {
                Ok(()) => {
                    out.http_chunks += 1;
                    if let Err(e) = self.cache.store(&r.hash, dest) {
                        tracing::warn!(input = name, error = %e, "artifact cache store failed");
                    }
                    return Ok(out);
                }
                Err(gg_client::client::ClientError::Status { status: 409, .. })
                    if self.cfg.p2p.enabled =>
                {
                    use_peers = true;
                }
                Err(e) => return Err(format!("download {name}: {e}")),
            }
        }
        debug_assert!(use_peers);
        // Swarm: this worker serves each chunk as soon as it has verified it, so it announces
        // itself as a holder before finishing, for the readers starting alongside it to find.
        if sharing && large {
            self.announce(token, &r.hash).await;
        }
        // An activation handed off before its durable copy (ADR-0026) exists only on its
        // producer: the server answers 409 until it has it. The fetch then goes through peers,
        // within a time limit, and gives up after that.
        let waited = Instant::now();
        loop {
            match crate::p2p::fetch_with_peers(
                &self.api,
                token,
                &self.key,
                &self.info.network_id,
                &self.info.server_public_key,
                r,
                &http_path,
                dest,
                &crate::p2p::FetchConfig {
                    max_peers: self.cfg.p2p.max_peers as usize,
                    allow_private_peers: self.cfg.p2p.allow_private_peers,
                    request_timeout_secs: self.cfg.p2p.request_timeout_secs,
                    parallel_chunks: self.cfg.p2p.parallel_chunks,
                    partials: self.peer_server.as_ref().map(|p| p.partials()),
                    self_fingerprint: self.peer_server.as_ref().map(|p| p.fingerprint()),
                    swarm_patience_ms: self.cfg.p2p.swarm_patience_ms,
                },
            )
            .await
            {
                Ok(stats) => {
                    out.peer_chunks += stats.chunks_from_peers;
                    out.http_chunks += stats.chunks_from_http;
                    out.bad_chunks += stats.bad_chunks;
                    if !stats.peers_dropped.is_empty() {
                        tracing::warn!(input = name, peers = ?stats.peers_dropped, "peers dropped for serving bytes that do not match the signed manifest");
                    }
                    break;
                }
                Err(e) => {
                    // The control plane is the durable source: a peer-assisted transfer that
                    // fails for any reason falls back to a plain resumable download.
                    tracing::debug!(input = name, error = %e, "peer-assisted download failed, falling back to the control plane");
                }
            }
            match self
                .api
                .download_retry(token, &http_path, r, self.cfg.max_download_bytes, dest)
                .await
            {
                Ok(()) => break,
                Err(gg_client::client::ClientError::Status { status: 409, .. })
                    if waited.elapsed().as_secs() < PENDING_INPUT_WAIT_SECS =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(format!("download {name}: {e}")),
            }
        }
        if let Err(e) = self.cache.store(&r.hash, dest) {
            tracing::warn!(input = name, error = %e, "artifact cache store failed");
        }
        // Now served from the cache: the download no longer needs to be listed.
        if let Some(server) = &self.peer_server {
            server.partials().end(&r.hash);
        }
        if sharing && large {
            self.announce(token, &r.hash).await;
        }
        Ok(out)
    }

    /// Declare an activation to the server (hash, size, chunk digests), keep it in the cache and
    /// announce holding it, and set aside a copy for the durable upload after submission.
    async fn declare(
        &mut self,
        token: &str,
        attempt: AttemptId,
        o: &crate::bundle::ReportOutput,
        bytes: &[u8],
        bundle: &Bundle,
    ) -> Result<(), String> {
        let chunk = self.info.p2p_chunk_bytes as usize;
        let digests: Vec<String> = bytes
            .chunks(chunk)
            .map(|c| Sha256Digest::of_bytes(c).to_hex())
            .collect();
        let path = bundle.outputs_dir().join(&o.file_name);
        self.cache
            .store(&o.hash, &path)
            .map_err(|e| format!("cache: {e}"))?;
        let keep_dir = self.cfg.work_dir.join("deferred");
        std::fs::create_dir_all(&keep_dir).map_err(|e| format!("deferred: {e}"))?;
        let keep = keep_dir.join(format!("{attempt}-{}", o.hash));
        std::fs::copy(&path, &keep).map_err(|e| format!("deferred: {e}"))?;
        let declared: Result<Option<serde_json::Value>, _> = self
            .api
            .post_json(
                token,
                &format!("/worker/attempts/{attempt}/outputs/{}/declare", o.hash),
                &serde_json::json!({ "size": o.size, "chunks": digests }),
            )
            .await;
        if let Err(e) = declared {
            let _ = std::fs::remove_file(&keep);
            return Err(e.to_string());
        }
        self.announce(token, &o.hash).await;
        self.deferred.push(DeferredUpload {
            attempt_id: attempt,
            hash: o.hash,
            size: o.size,
            path: keep,
        });
        Ok(())
    }

    /// Address other workers were last told to fetch chunks from, when sharing is on.
    pub async fn peer_addr(&self) -> Option<String> {
        self.peer_addr.lock().await.as_ref().map(|(a, _)| a.clone())
    }

    /// Decide the address to announce. In order: an explicit `advertise_addr`; a NAT-PMP mapping
    /// obtained from the local gateway; the bound address when it is concrete; otherwise the source
    /// address the control plane observed for this worker (useful behind a NAT that preserves the
    /// port). The control plane dials the address back before advertising it, so an unreachable
    /// worker simply shares nothing (ADR-0013).
    async fn candidate_addresses(&self, token: &str) -> Vec<Candidate> {
        let Some(server) = self.peer_server.as_ref() else {
            return Vec::new();
        };
        let bound = server.addr();
        let far = Duration::from_secs(3600);
        let mut out = Vec::new();

        // 1. What the contributor configured wins over anything discovered.
        if !self.cfg.p2p.advertise_addr.is_empty()
            && let Ok(a) = self.cfg.p2p.advertise_addr.parse::<std::net::SocketAddr>()
        {
            let addr = if a.port() == 0 {
                std::net::SocketAddr::new(a.ip(), bound.port()).to_string()
            } else {
                a.to_string()
            };
            out.push(Candidate::new(addr, "configured", far));
        }

        // 2. IPv6 first when the peer server can answer on it: there is no NAT to traverse, so no
        // mapping to obtain, renew or clean up. The control plane still dials the address back
        // before advertising it, which is what catches a firewall that blocks incoming IPv6.
        if self.cfg.p2p.ipv6
            && (bound.is_ipv6() || bound.ip().is_unspecified())
            && let Some(v6) = crate::nat_probe::global_ipv6()
        {
            let addr =
                std::net::SocketAddr::new(std::net::IpAddr::V6(v6), bound.port()).to_string();
            out.push(Candidate::new(addr, "ipv6", far));
        }

        // 3. Then the port-mapping protocols, for IPv4 behind a NAT.
        if self.cfg.p2p.nat_pmp {
            let gateway = self
                .cfg
                .p2p
                .nat_pmp_gateway
                .parse::<std::net::SocketAddr>()
                .ok();
            match crate::natpmp::map_port(
                bound.port(),
                self.cfg.p2p.nat_pmp_lifetime_secs,
                Duration::from_secs(2),
                gateway,
            )
            .await
            {
                Ok((external, mapping)) => {
                    tracing::info!(
                        %external,
                        lifetime_secs = mapping.lifetime_secs,
                        "gateway mapped a port for peer sharing (NAT-PMP)"
                    );
                    // Renew halfway through the granted lifetime, as RFC 6886 advises.
                    out.push(Candidate::new(
                        external.to_string(),
                        "nat-pmp",
                        Duration::from_secs((mapping.lifetime_secs.max(120) / 2) as u64),
                    ));
                }
                Err(e) => tracing::debug!(error = %e, "no NAT-PMP mapping"),
            }
        }
        if self.cfg.p2p.upnp
            && let Some(c) = self.map_port_with_upnp(bound).await
        {
            out.push(c);
        }

        // 4. A concrete bound address (laboratory network, single host).
        if !bound.ip().is_unspecified() {
            out.push(Candidate::new(bound.to_string(), "bound", far));
        }

        // 5. Last, the source address the control plane sees. Behind a NAT that preserves the port
        // this is the reachable one; over IPv6 it is simply this machine.
        if let Ok(me) = self
            .api
            .get_json::<serde_json::Value>(token, "/worker/me")
            .await
            && let Some(ip) = me["observed_ip"].as_str()
            && let Ok(ip) = ip.parse::<std::net::IpAddr>()
        {
            let addr = std::net::SocketAddr::new(ip, bound.port()).to_string();
            if !out.iter().any(|c| c.addr == addr) {
                out.push(Candidate::new(addr, "observed", Duration::from_secs(600)));
            }
        }
        out
    }

    /// Counters of the sharing side, when sharing is on.
    pub fn peer_stats(&self) -> Option<std::sync::Arc<crate::p2p::ShareStats>> {
        self.peer_server.as_ref().map(|s| s.stats())
    }

    /// Stop serving chunks and report what was shared. A port opened on the gateway is closed here:
    /// a worker that stops does not leave a hole behind.
    pub async fn stop_sharing(&mut self) {
        if let Some((gw, port)) = self.upnp_mapping.lock().await.take() {
            match gw.delete_port_mapping(port).await {
                Ok(()) => tracing::info!(port, "UPnP mapping deleted"),
                Err(e) => {
                    tracing::warn!(port, error = %e, "could not delete the UPnP mapping; it expires with its lease")
                }
            }
        }
        if let Some(server) = self.peer_server.take() {
            let (chunks, bytes, unauthorized, missing, budget) = server.stats().snapshot();
            tracing::info!(
                chunks,
                bytes,
                refused_unauthorized = unauthorized,
                refused_missing = missing,
                refused_budget = budget,
                "peer sharing stopped"
            );
            server.stop().await;
        }
    }

    /// Ask the gateway for a UPnP-IGD mapping and check it took. Any failure returns `None`, and
    /// the caller falls back to the address the control plane observed.
    async fn map_port_with_upnp(&self, bound: std::net::SocketAddr) -> Option<Candidate> {
        let gateway_ip = match self.cfg.p2p.nat_pmp_gateway.parse::<std::net::SocketAddr>() {
            Ok(a) => match a.ip() {
                std::net::IpAddr::V4(v4) => v4,
                std::net::IpAddr::V6(_) => return None,
            },
            Err(_) => crate::natpmp::default_gateway().ok()?,
        };
        let gw = match crate::upnp::discover(gateway_ip, Duration::from_secs(4)).await {
            Ok(g) => g,
            Err(e) => {
                tracing::info!(error = %e, "no UPnP gateway, using the observed address");
                return None;
            }
        };
        let local = crate::nat_probe::local_address_towards(gateway_ip).ok()?;
        let internal = std::net::SocketAddr::from((std::net::IpAddr::V4(local), bound.port()));
        let lease = self.cfg.p2p.nat_pmp_lifetime_secs;
        if let Err(e) = gw
            .add_port_mapping(internal, bound.port(), lease, "gpu-goblin peer")
            .await
        {
            tracing::info!(error = %e, "gateway refused a UPnP mapping");
            return None;
        }
        // Trust the gateway's own table, not our request.
        match gw.get_port_mapping(bound.port()).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::info!("the gateway accepted a UPnP mapping but does not list it");
                return None;
            }
            Err(e) => tracing::debug!(error = %e, "could not read the UPnP mapping back"),
        }
        let external = gw.external_address().await.ok()?;
        *self.upnp_mapping.lock().await = Some((gw, bound.port()));
        let addr = format!("{external}:{}", bound.port());
        tracing::info!(%addr, lease_secs = lease, "gateway mapped a port for peer sharing (UPnP-IGD)");
        Some(Candidate::new(
            addr,
            "upnp",
            Duration::from_secs((lease.max(120) / 2) as u64),
        ))
    }

    /// Tell the control plane this worker holds an artifact, if sharing is on. Failures are not
    /// fatal: peer distribution is an optimization, never a dependency.
    async fn announce(&self, token: &str, hash: &Sha256Digest) {
        let Some(server) = self.peer_server.as_ref() else {
            return;
        };
        let known = {
            let guard = self.peer_addr.lock().await;
            match guard.as_ref() {
                Some((addr, renew)) if Instant::now() < *renew => Some(addr.clone()),
                _ => None,
            }
        };
        if let Some(addr) = known {
            if let Err(e) = self
                .api
                .announce_artifact(token, hash, &addr, &server.fingerprint())
                .await
            {
                tracing::debug!(%hash, error = %e, "announce refused, resolving the address again");
                *self.peer_addr.lock().await = None;
            }
            return;
        }
        // Offer each candidate until the control plane accepts one. Its dial-back is the judge of
        // reachability: an address nobody can connect to is never advertised to other workers.
        for candidate in self.candidate_addresses(token).await {
            match self
                .api
                .announce_artifact(token, hash, &candidate.addr, &server.fingerprint())
                .await
            {
                Ok(a) => {
                    tracing::info!(addr = %a.addr, how = candidate.how, "announced a reachable address");
                    *self.peer_addr.lock().await = Some((candidate.addr, candidate.renew));
                    return;
                }
                Err(e) => tracing::info!(
                    addr = %candidate.addr,
                    how = candidate.how,
                    error = %e,
                    "the control plane could not use this address, trying the next one"
                ),
            }
        }
        tracing::info!("no reachable address for peer sharing: this worker only fetches");
    }

    /// Release the cache pins taken for an attempt.
    fn unpin_inputs(&self, claim: &ClaimResponse) {
        for r in claim.spec.definition.inputs.values() {
            self.cache.unpin(&r.hash);
        }
    }

    async fn prepare_and_execute(
        &mut self,
        token: &str,
        claim: &ClaimResponse,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Signed<ResultSubmissionV1>, String> {
        let a = &claim.assignment;
        a.verify(&self.info.network_id, Domain::Assignment)
            .map_err(|e| format!("assignment signature: {e}"))?;
        if a.signer != self.info.server_public_key {
            return Err("assignment not signed by the server key".into());
        }
        if a.body.worker_id != self.worker_id {
            return Err("assignment addressed to another worker".into());
        }
        let spec_hash = hash_message(&self.info.network_id, Domain::TaskSpec, &claim.spec)
            .map_err(|e| e.to_string())?;
        if spec_hash != a.body.spec_hash {
            return Err("spec hash does not match the assignment".into());
        }
        let phase_start = Instant::now();
        let task = gg_compute::validate_definition(&claim.spec.definition)
            .map_err(|e| format!("catalog: {e}"))?;
        let limits = &claim.spec.definition.resource_limits;
        if limits.max_ram_bytes > self.cfg.max_ram_bytes
            || limits.max_disk_bytes > self.cfg.max_disk_bytes
        {
            return Err("task exceeds local resource policy".into());
        }
        if self.engine.numeric_profile() != gg_compute::profiles::CPU_F32_FLEX_V1
            && !gg_compute::profiles::is_admitted(self.engine.numeric_profile())
        {
            return Err("local engine profile not admitted".into());
        }

        let bundle = Bundle::new(
            self.session_root.as_deref().unwrap_or(&self.cfg.work_dir),
            &attempt_dir_name(a.body.attempt_id),
        );
        bundle.create().map_err(|e| format!("bundle: {e}"))?;
        let spec_bytes =
            gg_protocol::canonical::to_bytes(&claim.spec).map_err(|e| e.to_string())?;
        std::fs::write(bundle.task_file(), &spec_bytes).map_err(|e| format!("bundle: {e}"))?;
        // Inputs of an attempt in progress are pinned: the bounded cache never evicts what the
        // executor is about to read.
        for r in claim.spec.definition.inputs.values() {
            self.cache.pin(&r.hash);
        }
        // All inputs at once: on a real network each one costs at least a round trip, which is
        // then paid only once.
        let me: &Self = self;
        let fetched =
            futures_util::future::join_all(claim.spec.definition.inputs.iter().map(|(name, r)| {
                let dest = bundle.input_path(name);
                async move { me.fetch_input(token, name, r, &dest).await }
            }))
            .await;
        let (mut cache_hits, mut peer_chunks, mut http_chunks, mut bad_chunks) = (0u32, 0, 0, 0);
        for f in fetched {
            let f = f?;
            cache_hits += f.cache_hit as u32;
            peer_chunks += f.peer_chunks;
            http_chunks += f.http_chunks;
            bad_chunks += f.bad_chunks;
        }
        tracing::info!(
            cache_hits,
            peer_chunks,
            http_chunks,
            bad_chunks,
            inputs = claim.spec.definition.inputs.len(),
            "inputs ready"
        );

        let fetch_ms = phase_start.elapsed().as_millis() as u64;
        let started = Instant::now();
        let mut report = self
            .run_executor(
                token,
                a.body.attempt_id,
                &bundle,
                limits.max_duration_secs as u64,
                limits.max_ram_bytes,
                (claim.spec.project_id, claim.spec.experiment_id),
                shutdown,
            )
            .await;
        // One local retry when the executor died but left a checkpoint: the attempt (and its lease)
        // stays the same; the executor resumes from `outputs/checkpoint` and reports `resumed_from_step`.
        if let Err(e) = &report {
            let checkpoint = bundle.outputs_dir().join("checkpoint").join("state.json");
            let lease_alive = self.api.heartbeat(token, a.body.attempt_id).await.is_ok();
            if checkpoint.exists()
                && lease_alive
                && !e.contains("shutdown")
                && !e.contains("lease lost")
            {
                tracing::warn!(error = %e, "executor failed with a local checkpoint: resuming from local checkpoint");
                report = self
                    .run_executor(
                        token,
                        a.body.attempt_id,
                        &bundle,
                        limits.max_duration_secs as u64,
                        limits.max_ram_bytes,
                        (claim.spec.project_id, claim.spec.experiment_id),
                        shutdown,
                    )
                    .await;
            }
        }
        let exec_wall_ms = started.elapsed().as_millis() as u64;
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                if !self.cfg.keep_failed_bundles {
                    bundle.remove();
                }
                return Err(e);
            }
        };
        if self.cfg.dev_faults.hold_before_submit_secs > 0 {
            let hold = Duration::from_secs(self.cfg.dev_faults.hold_before_submit_secs);
            tracing::warn!(
                secs = hold.as_secs(),
                "DEV FAULT: holding attempt before upload"
            );
            let until = tokio::time::Instant::now() + hold;
            while tokio::time::Instant::now() < until {
                tokio::time::sleep(
                    Duration::from_secs(self.cfg.heartbeat_secs.max(1))
                        .min(until - tokio::time::Instant::now()),
                )
                .await;
                if let Err(e) = self.api.heartbeat(token, a.body.attempt_id).await {
                    return Err(format!("lease lost while holding: {e}"));
                }
            }
        }
        let mut report = report;
        if self.cfg.dev_faults.corrupt_outputs {
            for o in &mut report.outputs {
                let mut bytes = std::fs::read(bundle.outputs_dir().join(&o.file_name))
                    .map_err(|e| format!("read output {}: {e}", o.name))?;
                if let Some(last) = bytes.last_mut() {
                    *last ^= 0x01;
                }
                std::fs::write(bundle.outputs_dir().join(&o.file_name), &bytes)
                    .map_err(|e| format!("write output {}: {e}", o.name))?;
                o.hash = Sha256Digest::of_bytes(&bytes);
                o.size = bytes.len() as u64;
                tracing::warn!(output = %o.name, "DEV FAULT: output corrupted before upload");
            }
        }
        let mut outputs = std::collections::BTreeMap::new();
        // Direct hand-off before the durable copy (ADR-0026): the activations of a stage task
        // are declared, then served by this worker; their upload to the server follows the
        // submission.
        let handoff = self.cfg.p2p.enabled
            && self.cfg.p2p.share
            && self.cfg.p2p.early_handoff
            && self.info.p2p_chunk_bytes > 0
            && matches!(
                claim.spec.definition.params,
                gg_protocol::messages::TaskParamsV1::GraphStage(_)
            );
        self.deferred.clear();
        // Check each output, declare the large activations, then upload the rest concurrently:
        // on a real network each upload costs at least a round trip.
        let mut to_upload = Vec::new();
        for o in &report.outputs {
            let bytes = std::fs::read(bundle.outputs_dir().join(&o.file_name))
                .map_err(|e| format!("read output {}: {e}", o.name))?;
            if bytes.len() as u64 != o.size || Sha256Digest::of_bytes(&bytes) != o.hash {
                return Err(format!(
                    "executor report does not match output file {}",
                    o.name
                ));
            }
            if o.size > limits.max_output_bytes {
                return Err(format!("output {} exceeds max_output_bytes", o.name));
            }
            outputs.insert(
                o.name.clone(),
                ArtifactRef {
                    hash: o.hash,
                    size: o.size,
                },
            );
            // A small artifact is uploaded right away: declaring and then serving it would cost
            // more than sending it (`p2p.min_bytes`).
            if handoff
                && o.size >= self.cfg.p2p.min_bytes
                && (o.name.starts_with("out.") || o.name.starts_with("din."))
            {
                match self
                    .declare(token, a.body.attempt_id, o, &bytes, &bundle)
                    .await
                {
                    Ok(()) => continue,
                    Err(e) => {
                        tracing::warn!(output = %o.name, error = %e, "declaration refused, uploading before submission")
                    }
                }
            }
            to_upload.push(o);
        }
        let me: &Self = self;
        let attempt_id = a.body.attempt_id;
        let uploaded = futures_util::future::join_all(to_upload.into_iter().map(|o| {
            let path = bundle.outputs_dir().join(&o.file_name);
            async move {
                me.api
                    .upload(
                        token,
                        &format!("/worker/attempts/{attempt_id}/outputs/{}", o.hash),
                        &path,
                        &o.hash,
                        o.size,
                        "ggt1-f32",
                    )
                    .await
                    .map_err(|e| format!("upload {}: {e}", o.name))?;
                // Kept in the cache: the next stage, if it runs here, reads it in place. A large
                // output is also announced: the next stage fetches it from this worker, verified
                // against the manifest the server signs from the durable copy (ADR-0026).
                match me.cache.store(&o.hash, &path) {
                    Ok(()) => {
                        if me.cfg.p2p.enabled && me.cfg.p2p.share && o.size >= me.cfg.p2p.min_bytes
                        {
                            me.announce(token, &o.hash).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(output = %o.name, error = %e, "output not cached")
                    }
                }
                Ok::<(), String>(())
            }
        }))
        .await;
        for u in uploaded {
            u?;
        }
        let _ = task;
        let body = ResultSubmissionV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.info.network_id.clone(),
            assignment_hash: a
                .body_hash(&self.info.network_id, Domain::Assignment)
                .map_err(|e| e.to_string())?,
            task_id: a.body.task_id,
            attempt_id: a.body.attempt_id,
            spec_hash: a.body.spec_hash,
            generation: a.body.generation,
            outputs,
            execution_profile: report.execution_profile.clone(),
            declared_metrics: report.metrics.clone(),
            submitted_at: UnixMillis::now(),
        };
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, profile = %body.execution_profile.numeric_profile, "execution finished");
        // Where a task's time goes: inputs, the executor process (including the compute time it
        // reports), output uploads.
        tracing::info!(
            fetch_ms,
            executor_ms = exec_wall_ms,
            compute_ms = body.execution_profile.duration_ms,
            upload_ms = (started.elapsed().as_millis() as u64).saturating_sub(exec_wall_ms),
            "attempt timed"
        );
        let signed = Signed::sign(&self.key, &self.info.network_id, Domain::Result, body)
            .map_err(|e| e.to_string())?;
        bundle.remove();
        Ok(signed)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_executor(
        &self,
        token: &str,
        attempt: AttemptId,
        bundle: &Bundle,
        max_secs: u64,
        max_ram: u64,
        scope: crate::session::Scope,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<ExecutorReport, String> {
        if self.session_root.is_some() {
            return self
                .run_in_session(token, attempt, bundle, max_secs, scope, shutdown)
                .await;
        }
        let exe = self.cfg.executor().map_err(|e| e.to_string())?;
        let mut cmd = match self.cfg.sandbox_mode() {
            crate::sandbox::SandboxMode::Bwrap => {
                let bwrap =
                    crate::sandbox::find_bwrap().ok_or("bubblewrap (bwrap) not found in PATH")?;
                let device = self.cfg.device_memory().unwrap_or(0).to_string();
                let mut inner = vec![
                    "run",
                    "--task-dir",
                    crate::bundle::SANDBOX_BUNDLE_DIR,
                    "--engine",
                    &self.cfg.engine,
                    "--restrict",
                    "--device-memory",
                    &device,
                ];
                if self.kernel_cache.is_some() {
                    inner.extend(["--kernel-cache", crate::bundle::SANDBOX_KERNEL_CACHE_DIR]);
                }
                let mut plan = crate::sandbox::plan_with(
                    &bwrap,
                    &exe,
                    &bundle.root,
                    self.engine,
                    &inner,
                    self.kernel_cache.as_deref(),
                    self.cfg.libtorch_dir.as_deref(),
                )?;
                if self.cgroups_available
                    && let Some(sr) = crate::sandbox::find_systemd_run()
                {
                    let limits = crate::sandbox::CgroupLimits {
                        memory_max: max_ram.saturating_add(512 << 20),
                        cpu_percent: if self.cfg.cpu_quota_percent > 0 {
                            Some(self.cfg.cpu_quota_percent)
                        } else {
                            None
                        },
                        tasks_max: 1024,
                    };
                    plan = plan.with_cgroup(&sr, limits);
                }
                crate::sandbox::command(&plan)
            }
            crate::sandbox::SandboxMode::None => {
                let mut cmd = tokio::process::Command::new(&exe);
                cmd.arg("run")
                    .arg("--task-dir")
                    .arg(&bundle.root)
                    .arg("--engine")
                    .arg(&self.cfg.engine)
                    .arg("--device-memory")
                    .arg(self.cfg.device_memory().unwrap_or(0).to_string())
                    .env_clear()
                    .env("NVIDIA_TF32_OVERRIDE", "0");
                if let Some(dir) = &self.cfg.libtorch_dir {
                    cmd.env("LD_LIBRARY_PATH", dir);
                }
                cmd
            }
        };
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn executor {}: {e}", exe.display()))?;
        let mut stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.take(64 * 1024).read_to_end(&mut buf).await;
            }
            String::from_utf8_lossy(&buf).into_owned()
        });
        let mut stdout = child.stdout.take();
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.take(64 * 1024).read_to_end(&mut buf).await;
            }
            String::from_utf8_lossy(&buf).into_owned()
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(max_secs.max(1));
        let mut hb = tokio::time::interval(Duration::from_secs(self.cfg.heartbeat_secs.max(5)));
        hb.tick().await;
        let crash_once = self.cfg.dev_faults.crash_executor_once
            && !bundle.outputs_dir().join(".crashed-once").exists();
        let mut crash_poll = tokio::time::interval(Duration::from_millis(50));
        let status = loop {
            tokio::select! {
                s = child.wait() => break s.map_err(|e| format!("wait executor: {e}"))?,
                _ = crash_poll.tick(), if crash_once => {
                    if bundle.outputs_dir().join("checkpoint").join("state.json").exists() {
                        let _ = std::fs::write(bundle.outputs_dir().join(".crashed-once"), b"");
                        tracing::warn!("DEV FAULT: killing the executor after its first checkpoint");
                        let _ = child.kill().await;
                    }
                }
                _ = hb.tick() => {
                    match self.api.heartbeat(token, attempt).await {
                        Ok(_) => {}
                        Err(ClientError::Status { status: 409, message }) => {
                            let _ = child.kill().await;
                            return Err(format!("lease lost: {message}"));
                        }
                        Err(e) => tracing::warn!(error = %e, "heartbeat failed"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.kill().await;
                    return Err(format!("executor exceeded max_duration_secs={max_secs}"));
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        let _ = child.kill().await;
                        return Err("shutdown requested".into());
                    }
                }
            }
        };
        let err_text = stderr_task.await.unwrap_or_default();
        let out_text = stdout_task.await.unwrap_or_default();
        if !out_text.trim().is_empty() {
            tracing::debug!(executor_stdout = %out_text.trim());
        }
        if !status.success() {
            // The whole end of the executor's error output stays in the local log: a panic's
            // location and message are further down than the one line sent to the server.
            let tail_start = err_text
                .char_indices()
                .rev()
                .nth(4095)
                .map_or(0, |(i, _)| i);
            tracing::warn!(executor_stderr = %err_text[tail_start..].trim(), "executor failed");
            let summary: String = err_text
                .lines()
                .last()
                .unwrap_or("")
                .chars()
                .take(180)
                .collect();
            return Err(format!("executor exited with {status}: {summary}"));
        }
        read_report(bundle)
    }

    /// The command of an executor session: `gg-executor serve` in the task sandbox, binding the
    /// session's bundle directory, under the worker's memory limit.
    fn session_command(&self, root: &std::path::Path) -> Result<tokio::process::Command, String> {
        let exe = self.cfg.executor().map_err(|e| e.to_string())?;
        let device = self.cfg.device_memory().unwrap_or(0).to_string();
        let max_file = self.cfg.max_disk_bytes.to_string();
        let mut cmd = match self.cfg.sandbox_mode() {
            crate::sandbox::SandboxMode::Bwrap => {
                let bwrap =
                    crate::sandbox::find_bwrap().ok_or("bubblewrap (bwrap) not found in PATH")?;
                let mut inner = vec![
                    "serve",
                    "--root",
                    crate::bundle::SANDBOX_BUNDLE_DIR,
                    "--engine",
                    &self.cfg.engine,
                    "--restrict",
                    "--device-memory",
                    &device,
                    "--max-file-bytes",
                    &max_file,
                ];
                if self.kernel_cache.is_some() {
                    inner.extend(["--kernel-cache", crate::bundle::SANDBOX_KERNEL_CACHE_DIR]);
                }
                let mut plan = crate::sandbox::plan_with(
                    &bwrap,
                    &exe,
                    root,
                    self.engine,
                    &inner,
                    self.kernel_cache.as_deref(),
                    self.cfg.libtorch_dir.as_deref(),
                )?;
                if self.cgroups_available
                    && let Some(sr) = crate::sandbox::find_systemd_run()
                {
                    let limits = crate::sandbox::CgroupLimits {
                        memory_max: self.cfg.max_ram_bytes.saturating_add(512 << 20),
                        cpu_percent: if self.cfg.cpu_quota_percent > 0 {
                            Some(self.cfg.cpu_quota_percent)
                        } else {
                            None
                        },
                        tasks_max: 1024,
                    };
                    plan = plan.with_cgroup(&sr, limits);
                }
                crate::sandbox::command(&plan)
            }
            crate::sandbox::SandboxMode::None => {
                let mut cmd = tokio::process::Command::new(&exe);
                cmd.arg("serve")
                    .arg("--root")
                    .arg(root)
                    .arg("--engine")
                    .arg(&self.cfg.engine)
                    .arg("--device-memory")
                    .arg(&device)
                    .arg("--max-file-bytes")
                    .arg(&max_file)
                    .env_clear()
                    .env("NVIDIA_TF32_OVERRIDE", "0");
                if let Some(dir) = &self.cfg.libtorch_dir {
                    cmd.env("LD_LIBRARY_PATH", dir);
                }
                cmd
            }
        };
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        );
        Ok(cmd)
    }

    /// Run one task in the executor session of its training run (ADR-0034), starting one if
    /// needed; the session is ended after any failure, a timeout, a lost lease or a shutdown.
    async fn run_in_session(
        &self,
        token: &str,
        attempt: AttemptId,
        bundle: &Bundle,
        max_secs: u64,
        scope: crate::session::Scope,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<ExecutorReport, String> {
        use crate::session::{Answer, Session};
        let root = self.session_root.as_deref().ok_or("no session root")?;
        let name = bundle
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or("bundle without a name")?;
        let mut guard = self.session.lock().await;
        let stale = guard
            .as_ref()
            .is_some_and(|s| s.scope != scope || s.tasks >= self.cfg.executor_session_max_tasks);
        if stale && let Some(old) = guard.take() {
            tracing::info!(
                tasks = old.tasks,
                "executor session ended (other run or task budget)"
            );
            old.kill().await;
        }
        if guard.is_none() {
            *guard = Some(Session::spawn(self.session_command(root)?, scope)?);
            tracing::info!("executor session started");
        }
        let session = guard.as_mut().expect("just started");
        session.send(&name).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(max_secs.max(1));
        let mut hb = tokio::time::interval(Duration::from_secs(self.cfg.heartbeat_secs.max(5)));
        hb.tick().await;
        let outcome: Result<(), String> = loop {
            tokio::select! {
                a = session.answer() => break match a {
                    Answer::Done => Ok(()),
                    Answer::Failed(e) => Err(format!("executor: {e}")),
                    Answer::Exited(tail) => {
                        let start = tail.char_indices().rev().nth(4095).map_or(0, |(i, _)| i);
                        tracing::warn!(executor_stderr = %&tail[start..], "executor session exited");
                        let last: String = tail.lines().last().unwrap_or("").chars().take(180).collect();
                        Err(format!("executor session exited: {last}"))
                    }
                },
                _ = hb.tick() => {
                    match self.api.heartbeat(token, attempt).await {
                        Ok(_) => {}
                        Err(ClientError::Status { status: 409, message }) => {
                            break Err(format!("lease lost: {message}"));
                        }
                        Err(e) => tracing::warn!(error = %e, "heartbeat failed"),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break Err(format!("executor exceeded max_duration_secs={max_secs}"));
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break Err("shutdown requested".into());
                    }
                }
            }
        };
        match outcome {
            Ok(()) => read_report(bundle),
            Err(e) => {
                // A session that failed a task, overran, or lost its lease is not reused.
                if let Some(s) = guard.take() {
                    s.kill().await;
                }
                Err(e)
            }
        }
    }
}

/// The executor's report for a bundle, with output file names checked.
fn read_report(bundle: &Bundle) -> Result<ExecutorReport, String> {
    let text = std::fs::read_to_string(bundle.report_file()).map_err(|e| format!("report: {e}"))?;
    let report: ExecutorReport = serde_json::from_str(&text).map_err(|e| format!("report: {e}"))?;
    for o in &report.outputs {
        if o.file_name.is_empty()
            || o.file_name.contains('/')
            || o.file_name.contains('\\')
            || o.file_name.starts_with('.')
        {
            return Err("executor reported an invalid output file name".into());
        }
    }
    Ok(report)
}

fn attempt_dir_name(id: AttemptId) -> String {
    id.to_string()
}
