//! The link to the workers of this machine: their local control API (ADR-0043), found through the
//! files each worker writes next to its key (`control.addr`, `control.token`). A machine with
//! several graphics cards runs one worker per card: the first in the base directory, the others
//! in `gpu1`, `gpu2`… under it. A thread of its own asks each for its status every two seconds
//! and carries out the owner's orders; the interface reads the last answers without waiting.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::i18n::{tr, trf};
use gg_client::worker_control::{Policy, Status};

/// An order of the owner.
#[derive(Clone, Debug)]
pub enum Order {
    /// Pause; `now`: abandon the running task too.
    Pause {
        now: bool,
    },
    Resume,
    SetPolicy(Policy),
}

/// What the interface shows.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The worker's last status.
    pub status: Option<Status>,
    /// Why the worker cannot be reached, or why an order failed.
    pub error: Option<String>,
    /// When the worker last answered.
    pub answered: Option<Instant>,
}

impl Snapshot {
    /// The worker answered within the last few seconds.
    pub fn connected(&self) -> bool {
        self.answered
            .is_some_and(|t| t.elapsed() < Duration::from_secs(6))
    }
}

/// Where the worker of this user keeps its key, policy and control files.
pub fn default_dir() -> PathBuf {
    crate::dirs::data_dir()
}

fn endpoint(dir: &Path) -> Result<(String, String), String> {
    let addr = std::fs::read_to_string(dir.join("control.addr"))
        .map_err(|_| tr("the worker is not running (or not installed)").to_owned())?;
    let token = std::fs::read_to_string(dir.join("control.token"))
        .map_err(|e| trf("control token: {why}", &[("why", &e)]))?;
    Ok((
        format!("http://{}/v1", addr.trim()),
        token.trim().to_owned(),
    ))
}

/// The workers of this machine: the base directory, then `gpu1`, `gpu2`… that hold a worker.
pub fn worker_dirs(base: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![base.to_path_buf()];
    let mut more: Vec<(u32, PathBuf)> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name.strip_prefix("gpu")?.parse().ok()?;
            let dir = e.path();
            (dir.join("worker.key").exists() || dir.join("control.addr").exists())
                .then_some((n, dir))
        })
        .collect();
    more.sort();
    dirs.extend(more.into_iter().map(|(_, d)| d));
    dirs
}

/// One worker as the interface sees it.
#[derive(Clone, Debug)]
pub struct Member {
    /// `GPU 0`, `GPU 1`… (or `Worker` for a machine with one).
    pub label: String,
    pub snap: Snapshot,
}

/// The link, shared by the interface and the tray.
#[derive(Clone)]
pub struct Link {
    base: PathBuf,
    shared: Arc<Mutex<Vec<(PathBuf, Snapshot)>>>,
    orders: tokio::sync::mpsc::UnboundedSender<(Option<usize>, Order)>,
    /// The runtime the link runs on (the tray runs there too).
    pub handle: tokio::runtime::Handle,
}

impl Link {
    /// Start the link to the workers under `base`; `changed` is called after every round of
    /// answers (to repaint the window).
    pub fn start(base: PathBuf, changed: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Option<usize>, Order)>();
        let shared: Arc<Mutex<Vec<(PathBuf, Snapshot)>>> = Arc::new(Mutex::new(Vec::new()));
        let (htx, hrx) = std::sync::mpsc::channel();
        let state = shared.clone();
        let root = base.clone();
        std::thread::Builder::new()
            .name("worker-link".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the link");
                let _ = htx.send(rt.handle().clone());
                rt.block_on(async move {
                    let http = reqwest::Client::builder()
                        .timeout(Duration::from_secs(5))
                        .build()
                        .expect("an HTTP client");
                    let mut first = true;
                    loop {
                        let order = if first {
                            first = false;
                            None
                        } else {
                            tokio::select! {
                                o = rx.recv() => match o { Some(o) => Some(o), None => return },
                                _ = tokio::time::sleep(Duration::from_secs(2)) => None,
                            }
                        };
                        let answers = ask(&http, &root, order).await;
                        {
                            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                            let old = std::mem::take(&mut *s);
                            for (d, r) in answers {
                                let mut snap = old
                                    .iter()
                                    .find(|(p, _)| *p == d)
                                    .map(|(_, sn)| sn.clone())
                                    .unwrap_or_default();
                                match r {
                                    Ok(status) => {
                                        snap.status = Some(status);
                                        snap.error = None;
                                        snap.answered = Some(Instant::now());
                                    }
                                    Err(e) => snap.error = Some(e),
                                }
                                s.push((d, snap));
                            }
                        }
                        changed();
                    }
                });
            })
            .expect("the link thread");
        Self {
            base,
            shared,
            orders: tx,
            handle: hrx.recv().expect("the link's runtime"),
        }
    }

    /// Give every worker an order (carried out at once, in the background).
    pub fn send(&self, order: Order) {
        let _ = self.orders.send((None, order));
    }

    /// Give one worker (by its place in [`Link::members`]) an order.
    pub fn send_to(&self, index: usize, order: Order) {
        let _ = self.orders.send((Some(index), order));
    }

    /// Where the workers' files are.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Each worker, labelled.
    pub fn members(&self) -> Vec<Member> {
        members_of(&self.shared.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The machine as one: the first worker's policy, the others' state folded in (see
    /// [`fold`]).
    pub fn snapshot(&self) -> Snapshot {
        fold(&self.members())
    }
}

/// `GPU 1` for the worker on card 1 (from its engine, `gpu:1` or `cuda:1`, else its place).
fn label(place: usize, many: bool, snap: &Snapshot) -> String {
    let card = snap
        .status
        .as_ref()
        .and_then(|s| s.engine.split_once(':'))
        .and_then(|(_, n)| n.parse::<usize>().ok());
    match (card, many) {
        (Some(n), _) => format!("GPU {n}"),
        (None, true) => format!("GPU {place}"),
        (None, false) => "Worker".to_owned(),
    }
}

/// Several workers as one: connected if any is; the status of the busiest (computing, then
/// ready, then waiting, then paused), with the counters of all added up; the policy of the first.
pub fn fold(members: &[Member]) -> Snapshot {
    let live: Vec<&Member> = members.iter().filter(|m| m.snap.connected()).collect();
    let Some(first) = live.first() else {
        return members.first().map(|m| m.snap.clone()).unwrap_or_default();
    };
    let rank = |m: &&Member| match m.snap.status.as_ref() {
        Some(s) if s.state == "computing" => 0,
        Some(s) if s.state == "ready" => 1,
        Some(s) if s.waiting.is_some() && !s.policy.paused => 2,
        _ => 3,
    };
    let lead = live
        .iter()
        .min_by_key(|m| rank(m))
        .copied()
        .unwrap_or(first);
    let mut out = lead.snap.clone();
    if let (Some(s), Some(p)) = (out.status.as_mut(), first.snap.status.as_ref()) {
        s.policy = p.policy.clone();
        let all: Vec<&Status> = live.iter().filter_map(|m| m.snap.status.as_ref()).collect();
        s.submitted = all.iter().map(|x| x.submitted).sum();
        s.abandoned = all.iter().map(|x| x.abandoned).sum();
        s.accepted = all
            .iter()
            .map(|x| x.accepted)
            .sum::<Option<u64>>()
            .or(s.accepted);
        let earned: Option<u64> = all
            .iter()
            .map(|x| x.earned.as_deref().and_then(|e| e.parse::<u64>().ok()))
            .sum();
        if let Some(e) = earned {
            s.earned = Some(e.to_string());
        }
        // Paused only when every worker is.
        s.policy.paused = all.iter().all(|x| x.policy.paused);
    }
    out
}

/// Ask every worker under `base` for its status, one after the other, first giving the order to
/// those it is for (`None`: all, `Some(k)`: the `k`-th of [`worker_dirs`]).
pub async fn ask(
    http: &reqwest::Client,
    base: &Path,
    order: Option<(Option<usize>, Order)>,
) -> Vec<(PathBuf, Result<Status, String>)> {
    let mut out = Vec::new();
    for (k, d) in worker_dirs(base).into_iter().enumerate() {
        let o = order
            .as_ref()
            .filter(|(only, _)| only.is_none_or(|i| i == k))
            .map(|(_, o)| o.clone());
        let r = exchange(http, &d, o).await;
        out.push((d, r));
    }
    out
}

/// Label the workers: `GPU 0`, `GPU 1`… (`Worker` alone).
pub fn members_of(snaps: &[(PathBuf, Snapshot)]) -> Vec<Member> {
    let many = snaps.len() > 1;
    snaps
        .iter()
        .enumerate()
        .map(|(k, (_, snap))| Member {
            label: label(k, many, snap),
            snap: snap.clone(),
        })
        .collect()
}

async fn exchange(
    http: &reqwest::Client,
    dir: &Path,
    order: Option<Order>,
) -> Result<Status, String> {
    let (base, token) = endpoint(dir)?;
    let request = match &order {
        None => http.get(format!("{base}/status")),
        Some(Order::Pause { now }) => http
            .post(format!("{base}/pause"))
            .json(&serde_json::json!({ "now": now })),
        Some(Order::Resume) => http.post(format!("{base}/resume")),
        Some(Order::SetPolicy(p)) => http.put(format!("{base}/policy")).json(p),
    };
    let r = request.bearer_auth(&token).send().await.map_err(|e| {
        let why = std::error::Error::source(&e).map_or_else(|| e.to_string(), |s| s.to_string());
        trf("the worker does not answer ({why})", &[("why", &why)])
    })?;
    let code = r.status();
    if !code.is_success() {
        let body: serde_json::Value = r.json().await.unwrap_or_default();
        return Err(body["message"].as_str().map_or_else(
            || trf("the worker answered {code}", &[("code", &code)]),
            str::to_owned,
        ));
    }
    // A policy answer is the policy: ask for the whole status after it.
    if matches!(order, Some(Order::SetPolicy(_))) {
        return Box::pin(exchange(http, dir, None)).await;
    }
    r.json::<Status>().await.map_err(|e| format!("status: {e}"))
}
