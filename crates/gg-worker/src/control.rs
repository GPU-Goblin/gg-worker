//! The owner's control of the worker (ADR-0043): a policy (pause, only when idle, hours, GPU
//! temperature, yielding the GPU to other programs, memory), the readings it is checked against,
//! the worker's status, and the local control API the desktop application talks to.
//!
//! The policy is the owner's and is local: the server never sees it. It only decides whether the
//! worker claims a new task; a task already running finishes, unless the owner asks for an
//! immediate pause, which abandons it properly (the server reassigns it at once, no sanction).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Deserialize;

pub use gg_client::worker_control::*;

/// Local day of the week (0 = Monday) and minutes since midnight, in the machine's time zone.
#[cfg(windows)]
pub fn local_now() -> (u32, u32) {
    crate::winsys::local_now()
}

/// Local day of the week (0 = Monday) and minutes since midnight: from `date`, which knows the
/// machine's time zone; UTC if it cannot be run.
#[cfg(not(windows))]
pub fn local_now() -> (u32, u32) {
    if let Some(out) = output("date", &["+%u %H %M"]) {
        let f: Vec<u32> = out
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        if let [d, h, m] = f[..]
            && (1..=7).contains(&d)
            && h < 24
            && m < 60
        {
            return (d - 1, h * 60 + m);
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 1970-01-01 was a Thursday.
    (
        ((secs / 86_400 + 3) % 7) as u32,
        ((secs % 86_400) / 60) as u32,
    )
}

/// This process's user id.
#[cfg(not(windows))]
fn my_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").ok().map(|m| m.uid())
    }
    #[cfg(not(unix))]
    None
}

/// Run a command for a short while and return its standard output.
fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read the machine, without privileges. `gpu_index`: the card the worker computes on.
pub fn read(gpu_index: usize) -> Readings {
    let mut r = Readings::default();
    // The user's session on Windows: locked, seconds since the last input.
    #[cfg(windows)]
    {
        r.locked = crate::winsys::locked();
        r.idle_secs = crate::winsys::idle_secs();
    }
    // The user's graphical session (logind): locked, idle since when.
    #[cfg(not(windows))]
    if let Some(list) = output("loginctl", &["list-sessions", "--no-legend"]) {
        let uid = my_uid().map(|u| u.to_string()).unwrap_or_default();
        let session = list.lines().find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 4 && f[1] == uid && f[3].starts_with("seat")).then(|| f[0].to_owned())
        });
        if let Some(s) = session
            && let Some(props) = output(
                "loginctl",
                &[
                    "show-session",
                    &s,
                    "-p",
                    "LockedHint",
                    "-p",
                    "IdleHint",
                    "-p",
                    "IdleSinceHint",
                ],
            )
        {
            let get = |k: &str| {
                props
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_owned))
            };
            r.locked = get("LockedHint").map(|v| v == "yes");
            let idle = get("IdleHint").map(|v| v == "yes");
            let since = get("IdleSinceHint").and_then(|v| v.parse::<u64>().ok());
            r.idle_secs = match (idle, since) {
                (Some(true), Some(us)) if us > 0 => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as u64)
                        .unwrap_or(0);
                    Some(now.saturating_sub(us) / 1_000_000)
                }
                (Some(false), _) => Some(0),
                _ => None,
            };
        }
    }
    // NVIDIA: temperature and utilization of the card.
    if let Some(csv) = output(
        "nvidia-smi",
        &[
            "--query-gpu=temperature.gpu,utilization.gpu",
            "--format=csv,noheader,nounits",
        ],
    ) && let Some(line) = csv.lines().nth(gpu_index)
    {
        let mut f = line.split(',').map(|x| x.trim().parse::<u32>().ok());
        r.gpu_temp_c = f.next().flatten();
        r.gpu_busy_percent = f.next().flatten();
    } else if let Some(card) = amd_card(gpu_index) {
        // AMD and others: the DRM and hwmon sysfs files.
        r.gpu_busy_percent = std::fs::read_to_string(card.join("gpu_busy_percent"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        r.gpu_temp_c = std::fs::read_dir(card.join("hwmon"))
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .and_then(|e| std::fs::read_to_string(e.path().join("temp1_input")).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|milli| milli / 1000);
    }
    r
}

fn amd_card(index: usize) -> Option<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
                && p.join("device/gpu_busy_percent").exists()
        })
        .map(|p| p.join("device"))
        .collect();
    cards.sort();
    cards.into_iter().nth(index)
}

/// The shared state between the agent, the control API and the policy file.
pub struct Control {
    path: PathBuf,
    policy: RwLock<Policy>,
    status: Mutex<Status>,
    /// Woken when the policy changes.
    pub changed: tokio::sync::Notify,
    /// Raised by an immediate pause: the running task is abandoned.
    abort: tokio::sync::watch::Sender<u64>,
}

impl Control {
    /// Load the policy at `path` (defaults when absent).
    pub fn new(path: PathBuf, engine: &str) -> anyhow::Result<Arc<Self>> {
        let policy = Policy::load(&path).map_err(anyhow::Error::msg)?;
        Ok(Arc::new(Self {
            status: Mutex::new(Status {
                state: "starting".into(),
                waiting: None,
                task: None,
                readings: Readings::default(),
                submitted: 0,
                abandoned: 0,
                earned: None,
                accepted: None,
                worker_id: None,
                engine: engine.to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                policy: policy.clone(),
            }),
            policy: RwLock::new(policy),
            path,
            changed: tokio::sync::Notify::new(),
            abort: tokio::sync::watch::channel(0).0,
        }))
    }

    pub fn policy(&self) -> Policy {
        self.policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the policy and write it.
    pub fn set_policy(&self, p: Policy) -> anyhow::Result<()> {
        p.save(&self.path).map_err(anyhow::Error::msg)?;
        *self.policy.write().unwrap_or_else(|e| e.into_inner()) = p.clone();
        self.update(|s| s.policy = p);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Pause; `now`: abandon the running task too.
    pub fn pause(&self, now: bool) -> anyhow::Result<()> {
        let mut p = self.policy();
        p.paused = true;
        self.set_policy(p)?;
        if now {
            self.abort.send_modify(|n| *n += 1);
        }
        Ok(())
    }

    pub fn resume(&self) -> anyhow::Result<()> {
        let mut p = self.policy();
        p.paused = false;
        self.set_policy(p)
    }

    /// A receiver that changes on every immediate pause.
    pub fn aborts(&self) -> tokio::sync::watch::Receiver<u64> {
        self.abort.subscribe()
    }

    pub fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Read the policy file again if someone else changed it.
    pub fn reload(&self) {
        if let Ok(p) = Policy::load(&self.path)
            && p != self.policy()
        {
            *self.policy.write().unwrap_or_else(|e| e.into_inner()) = p.clone();
            self.update(|s| s.policy = p);
            self.changed.notify_waiters();
        }
    }
}

/// The token of the control API at `path`, created (32 random bytes, hex, readable by the user
/// alone) when absent.
pub fn token(path: &Path) -> anyhow::Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim().to_owned();
        if t.len() == 64 {
            return Ok(t);
        }
    }
    let mut bytes = [0u8; 32];
    getrandom(&mut bytes)?;
    let t = hex::encode(bytes);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(path)?.write_all(t.as_bytes())?;
    Ok(t)
}

fn getrandom(buf: &mut [u8]) -> anyhow::Result<()> {
    getrandom::fill(buf).map_err(|e| anyhow::anyhow!("system random generator: {e}"))
}

/// Serve the control API on `listen` (loopback only) until `stop`.
pub async fn serve(
    ctl: Arc<Control>,
    listen: &str,
    token: String,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<std::net::SocketAddr> {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};

    let addr: std::net::SocketAddr = listen.parse()?;
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "the control API listens on this machine only (127.0.0.1)"
    );
    #[derive(Clone)]
    struct S {
        ctl: Arc<Control>,
        token: Arc<String>,
    }
    type Refused = (StatusCode, Json<serde_json::Value>);
    fn allowed(s: &S, h: &HeaderMap) -> Result<(), Refused> {
        let given = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        // Constant time: the comparison does not stop at the first difference.
        let ok = given.len() == s.token.len()
            && given
                .bytes()
                .zip(s.token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if ok {
            Ok(())
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "message": "the control token is required" })),
            ))
        }
    }
    fn failed(e: anyhow::Error) -> Refused {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "message": format!("{e:#}") })),
        )
    }
    #[derive(Deserialize)]
    struct PauseReq {
        #[serde(default)]
        now: bool,
    }
    async fn status(State(s): State<S>, h: HeaderMap) -> Result<Json<Status>, Refused> {
        allowed(&s, &h)?;
        Ok(Json(s.ctl.status()))
    }
    async fn get_policy(State(s): State<S>, h: HeaderMap) -> Result<Json<Policy>, Refused> {
        allowed(&s, &h)?;
        Ok(Json(s.ctl.policy()))
    }
    async fn put_policy(
        State(s): State<S>,
        h: HeaderMap,
        Json(p): Json<Policy>,
    ) -> Result<Json<Policy>, Refused> {
        allowed(&s, &h)?;
        s.ctl.set_policy(p).map_err(failed)?;
        Ok(Json(s.ctl.policy()))
    }
    async fn pause(
        State(s): State<S>,
        h: HeaderMap,
        Json(r): Json<PauseReq>,
    ) -> Result<Json<Status>, Refused> {
        allowed(&s, &h)?;
        s.ctl.pause(r.now).map_err(failed)?;
        Ok(Json(s.ctl.status()))
    }
    async fn resume(State(s): State<S>, h: HeaderMap) -> Result<Json<Status>, Refused> {
        allowed(&s, &h)?;
        s.ctl.resume().map_err(failed)?;
        Ok(Json(s.ctl.status()))
    }
    let app = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/policy", get(get_policy).put(put_policy))
        .route("/v1/pause", post(pause))
        .route("/v1/resume", post(resume))
        .with_state(S {
            ctl,
            token: Arc::new(token),
        });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while stop.changed().await.is_ok() {
                    if *stop.borrow() {
                        break;
                    }
                }
            })
            .await;
    });
    Ok(bound)
}

/// How often the readings are taken while waiting.
pub const RECHECK: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    fn w(days: &[&str], from: &str, to: &str) -> Window {
        Window {
            days: days.iter().map(|d| d.to_string()).collect(),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn windows_cover_their_hours_and_nights() {
        let mut p = Policy::default();
        assert!(p.in_schedule(2, 600), "no window: always");
        p.schedule = vec![w(&["mon", "tue"], "09:00", "17:00")];
        assert!(p.in_schedule(0, 9 * 60));
        assert!(!p.in_schedule(0, 17 * 60));
        assert!(!p.in_schedule(2, 10 * 60), "wednesday");
        // A night window: friday 22:00 to saturday 07:00.
        p.schedule = vec![w(&["fri"], "22:00", "07:00")];
        assert!(p.in_schedule(4, 23 * 60));
        assert!(p.in_schedule(5, 6 * 60), "saturday morning");
        assert!(!p.in_schedule(5, 8 * 60));
        assert!(
            !p.in_schedule(4, 6 * 60),
            "friday morning belongs to thursday night"
        );
        p.schedule = vec![w(&["xyz"], "22:00", "07:00")];
        assert!(p.validate().is_err());
        p.schedule = vec![w(&[], "25:00", "07:00")];
        assert!(p.validate().is_err());
    }

    #[test]
    fn the_gate_follows_the_owner() {
        let r = Readings {
            locked: Some(false),
            idle_secs: Some(0),
            gpu_temp_c: Some(60),
            gpu_busy_percent: Some(3),
        };
        let mut p = Policy::default();
        assert_eq!(gate(&p, &r, 0, 600), None);
        p.paused = true;
        assert_eq!(gate(&p, &r, 0, 600), Some(Waiting::Paused));
        p.paused = false;
        p.only_when_idle = true;
        assert_eq!(gate(&p, &r, 0, 600), Some(Waiting::NotIdle));
        let away = Readings {
            idle_secs: Some(400),
            ..r.clone()
        };
        assert_eq!(gate(&p, &away, 0, 600), None);
        let locked = Readings {
            locked: Some(true),
            ..r.clone()
        };
        assert_eq!(gate(&p, &locked, 0, 600), None);
        // No session at all (a machine without a screen): idle.
        assert_eq!(gate(&p, &Readings::default(), 0, 600), None);
        p.only_when_idle = false;
        p.gpu_max_temp_c = Some(60);
        assert_eq!(
            gate(&p, &r, 0, 600),
            Some(Waiting::GpuHot {
                temp_c: 60,
                max_c: 60
            })
        );
        p.gpu_max_temp_c = None;
        let gaming = Readings {
            gpu_busy_percent: Some(85),
            ..r.clone()
        };
        assert_eq!(
            gate(&p, &gaming, 0, 600),
            Some(Waiting::GpuInUse { percent: 85 })
        );
        p.yield_to_other_gpu_apps = false;
        assert_eq!(gate(&p, &gaming, 0, 600), None);
        p.schedule = vec![w(&[], "22:00", "06:00")];
        assert_eq!(gate(&p, &r, 0, 600), Some(Waiting::OutsideSchedule));
    }

    #[test]
    fn the_policy_round_trips_and_the_token_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        assert_eq!(Policy::load(&path).unwrap(), Policy::default());
        let p = Policy {
            paused: true,
            schedule: vec![w(&["sat", "sun"], "08:00", "20:00")],
            gpu_max_temp_c: Some(75),
            ..Policy::default()
        };
        p.save(&path).unwrap();
        assert_eq!(Policy::load(&path).unwrap(), p);
        let t = token(&dir.path().join("control.token")).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(token(&dir.path().join("control.token")).unwrap(), t);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("control.token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
