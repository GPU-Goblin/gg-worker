//! The contributor's account, as the application holds it: signed in with a code sent by e-mail
//! (no password; the account is created by the first right code), its session kept in
//! `~/.config/gpu-goblin/account.toml` (readable by the owner alone), used to fetch the enrollment
//! code that attaches this computer, and the account's machines. The server's JSON contributor
//! API (`/contributor/v1`), the same account and rules as the site's pages.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The grid, unless the worker or the owner names another.
pub const DEFAULT_SERVER: &str = "https://grid.gpugoblin.com";

/// A signed-in account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub server: String,
    pub email: String,
    pub session: String,
    /// Seconds since 1970 when the session ends.
    pub expires_at: u64,
}

impl Account {
    pub fn expired(&self) -> bool {
        now() >= self.expires_at
    }
}

/// One of the account's machines.
#[derive(Clone, Debug, Deserialize)]
pub struct Machine {
    pub worker_id: String,
    pub name: String,
    pub working: bool,
    pub accepted: i64,
}

/// What the server says of the account.
#[derive(Clone, Debug, Deserialize)]
pub struct Me {
    pub email: String,
    pub suspended: bool,
    #[serde(default)]
    pub suspended_reason: String,
    pub machines: Vec<Machine>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn config_dir() -> Option<PathBuf> {
    crate::dirs::config_dir()
}

/// Where the session is kept.
pub fn path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("account.toml"))
}

/// The account saved at `path`, if any.
pub fn load_from(path: &Path) -> Option<Account> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The account saved, still valid.
pub fn load() -> Option<Account> {
    path().and_then(|p| load_from(&p)).filter(|a| !a.expired())
}

/// Keep `account` at `path`, readable by the owner alone.
pub fn save_to(path: &Path, account: &Account) -> Result<(), String> {
    let text = toml::to_string(account).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.new");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        let mut f = o.open(&tmp).map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn save(account: &Account) -> Result<(), String> {
    save_to(&path().ok_or("no configuration directory")?, account)
}

/// Forget the session here.
pub fn forget() {
    if let Some(p) = path() {
        let _ = std::fs::remove_file(p);
    }
}

/// The grid this computer lends to: the worker's (`worker.toml`), else `GG_SERVER_URL`, else
/// the default.
pub fn server() -> String {
    let from_worker = config_dir()
        .and_then(|d| std::fs::read_to_string(d.join("worker.toml")).ok())
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .and_then(|t| t.get("server_url")?.as_str().map(str::to_owned));
    from_worker
        .or_else(|| std::env::var("GG_SERVER_URL").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// A server address the application accepts: https, or http on this machine only.
pub fn check_server(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.starts_with(p));
    if !(url.starts_with("https://") || local) {
        return Err("https only".into());
    }
    reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    Ok(url.to_owned())
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

/// Why a call failed, in the owner's words (English: the caller translates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The server could not be reached.
    Unreachable(String),
    /// The session ended: sign in again.
    SignedOut,
    /// A wrong, expired or used code.
    WrongCode,
    /// Sign-up is not open on this server.
    Closed,
    /// The server refused, with its reason.
    Refused(String),
}

async fn call(
    method: reqwest::Method,
    server: &str,
    path: &str,
    session: Option<&str>,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, Failure> {
    let http = client().map_err(Failure::Unreachable)?;
    let mut req = http.request(method, format!("{server}/contributor/v1{path}"));
    if let Some(s) = session {
        req = req.bearer_auth(s);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.map_err(|e| {
        let why = std::error::Error::source(&e).map_or_else(|| e.to_string(), |s| s.to_string());
        Failure::Unreachable(why)
    })?;
    let code = r.status();
    let value: serde_json::Value = r.json().await.unwrap_or_default();
    let message = value["message"].as_str().unwrap_or_default().to_owned();
    match code.as_u16() {
        200..=299 => Ok(value),
        401 => Err(Failure::SignedOut),
        403 if message.contains("code is wrong") => Err(Failure::WrongCode),
        404 if message.contains("not open") => Err(Failure::Closed),
        404 => Err(Failure::Unreachable(format!(
            "{server} is not a GPU Goblin grid"
        ))),
        _ => Err(Failure::Refused(if message.is_empty() {
            code.to_string()
        } else {
            message
        })),
    }
}

/// Ask for a code sent to `email`.
pub async fn send_code(server: &str, email: &str) -> Result<(), Failure> {
    call(
        reqwest::Method::POST,
        server,
        "/code",
        None,
        Some(serde_json::json!({ "email": email })),
    )
    .await
    .map(|_| ())
}

/// Sign in (or up) with the code received.
pub async fn sign_in(server: &str, email: &str, code: &str) -> Result<Account, Failure> {
    let v = call(
        reqwest::Method::POST,
        server,
        "/session",
        None,
        Some(serde_json::json!({ "email": email, "code": code.trim() })),
    )
    .await?;
    let session = v["session"]
        .as_str()
        .ok_or_else(|| Failure::Refused("no session in the answer".into()))?;
    Ok(Account {
        server: server.to_owned(),
        email: v["email"].as_str().unwrap_or(email).to_owned(),
        session: session.to_owned(),
        expires_at: now() + v["expires_in_secs"].as_u64().unwrap_or(3600),
    })
}

/// The account and its machines.
pub async fn me(a: &Account) -> Result<Me, Failure> {
    let v = call(
        reqwest::Method::GET,
        &a.server,
        "/me",
        Some(&a.session),
        None,
    )
    .await?;
    serde_json::from_value(v).map_err(|e| Failure::Refused(e.to_string()))
}

/// A new enrollment code for this computer (the previous one stops attaching machines; those
/// attached keep working).
pub async fn enrollment(a: &Account) -> Result<String, Failure> {
    let v = call(
        reqwest::Method::POST,
        &a.server,
        "/enrollment",
        Some(&a.session),
        None,
    )
    .await?;
    v["enrollment_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Failure::Refused("no enrollment code in the answer".into()))
}

/// End the session on the server too.
pub async fn sign_out(a: &Account) {
    let _ = call(
        reqwest::Method::POST,
        &a.server,
        "/logout",
        Some(&a.session),
        None,
    )
    .await;
}

/// A call running in the background, its result read by the interface.
pub struct Pending<T>(Arc<Mutex<Option<T>>>);

impl<T: Send + 'static> Pending<T> {
    /// Run `f` on `rt`; `done` is called when it ends (to repaint).
    pub fn spawn(
        rt: &tokio::runtime::Handle,
        f: impl std::future::Future<Output = T> + Send + 'static,
        done: impl Fn() + Send + 'static,
    ) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let s = slot.clone();
        rt.spawn(async move {
            let out = f.await;
            *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
            done();
        });
        Self(slot)
    }

    /// The result, once (then `None`).
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_kept_for_the_owner_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gpu-goblin").join("account.toml");
        let a = Account {
            server: "https://grid.example".into(),
            email: "ada@example.org".into(),
            session: "ggp_secret".into(),
            expires_at: now() + 60,
        };
        save_to(&p, &a).unwrap();
        assert_eq!(load_from(&p), Some(a.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!a.expired());
        assert!(Account { expires_at: 1, ..a }.expired());
    }

    #[test]
    fn only_https_or_this_machine() {
        assert_eq!(
            check_server(" https://grid.gpugoblin.com/ ").unwrap(),
            "https://grid.gpugoblin.com"
        );
        assert!(check_server("http://127.0.0.1:8080").is_ok());
        assert!(check_server("http://grid.gpugoblin.com").is_err());
        assert!(check_server("ftp://x").is_err());
    }
}
