//! On Windows the application starts the workers (on Linux, systemd user units do): one per
//! configuration (`worker.toml`, `worker-gpu1.toml`…), without a console window, their output in
//! `worker.log` next to their key. A worker that stopped is started again at the next check. The
//! workers keep running when the application's window closes, as on Linux; the owner's pause is
//! the worker's own policy.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// A log grown past this is moved aside (one old copy kept).
const LOG_MAX: u64 = 16 << 20;

/// The workers' configurations, the first card's first.
pub fn configs(config_dir: &Path) -> Vec<PathBuf> {
    let mut more: Vec<(u32, PathBuf)> = std::fs::read_dir(config_dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let n = name.strip_prefix("worker-gpu")?.strip_suffix(".toml")?;
                    Some((n.parse().ok()?, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    more.sort();
    let first = config_dir.join("worker.toml");
    first
        .is_file()
        .then_some(first)
        .into_iter()
        .chain(more.into_iter().map(|(_, p)| p))
        .collect()
}

/// The directory of a worker's key, where it writes `control.addr` and its log.
fn data_dir(config: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    Path::new(v.get("key_path")?.as_str()?)
        .parent()
        .map(Path::to_path_buf)
}

/// Whether the worker of this data directory answers on its control address.
fn answers(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("control.addr"))
        .ok()
        .and_then(|a| a.trim().parse::<std::net::SocketAddr>().ok())
        .is_some_and(|a| {
            std::net::TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok()
        })
}

/// The workers this application started.
#[derive(Default)]
pub struct Workers {
    children: Vec<(PathBuf, Child)>,
}

impl Workers {
    /// Start every configured worker that is not running. Only an installed application (the
    /// worker next to it) starts anything.
    pub fn check(&mut self) {
        let Some(worker) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("gg-worker.exe")))
            .filter(|w| w.is_file())
        else {
            return;
        };
        let Some(config_dir) = crate::dirs::config_dir() else {
            return;
        };
        self.children
            .retain_mut(|(_, c)| matches!(c.try_wait(), Ok(None)));
        for config in configs(&config_dir) {
            if self.children.iter().any(|(c, _)| *c == config) {
                continue;
            }
            let Some(dir) = data_dir(&config) else {
                tracing::warn!(config = %config.display(), "no key_path: worker not started");
                continue;
            };
            if answers(&dir) {
                continue;
            }
            match start(&worker, &config, &dir) {
                Ok(child) => {
                    tracing::info!(config = %config.display(), "worker started");
                    self.children.push((config, child));
                }
                Err(e) => {
                    tracing::warn!(config = %config.display(), error = %e, "worker not started")
                }
            }
        }
    }
}

fn start(worker: &Path, config: &Path, dir: &Path) -> std::io::Result<Child> {
    std::fs::create_dir_all(dir)?;
    let log = dir.join("worker.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::rename(&log, dir.join("worker.log.1"));
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let err = out.try_clone()?;
    Command::new(worker)
        .arg("--config")
        .arg(config)
        .arg("run")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
}

/// Check the workers now and every minute, on a thread of their own.
pub fn keep_running() {
    let _ = std::thread::Builder::new()
        .name("workers".into())
        .spawn(|| {
            let mut w = Workers::default();
            loop {
                w.check();
                std::thread::sleep(Duration::from_secs(60));
            }
        });
}
