//! Where the application and the worker keep their files: the XDG directories on Linux,
//! `%APPDATA%` (settings) and `%LOCALAPPDATA%` (the worker's key, policy and control files) on
//! Windows.

use std::path::PathBuf;

/// Settings: `account.toml`, `desk.toml`, and the worker's `worker.toml`.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("GPU Goblin"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("gpu-goblin"))
}

/// The worker's own files (key, policy, `control.addr`, `control.token`); one subdirectory per
/// further graphics card.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("GG_WORKER_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(windows)
        && let Some(d) = std::env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(d).join("GPU Goblin");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gpu-goblin")
}
