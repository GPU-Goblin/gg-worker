//! The contributor's desktop application (ADR-0043): the link to the worker, the window, the
//! account, the setup, the tray, its languages.

#![deny(unsafe_code)]

pub mod about;
pub mod account;
pub mod app;
pub mod console;
pub mod dirs;
pub mod i18n;
pub mod install;
#[cfg(windows)]
pub mod install_windows;
pub mod link;
pub mod logo;
pub mod prefs;
pub mod setup;
pub mod theme;
#[cfg(target_os = "linux")]
pub mod tray;
#[cfg(windows)]
#[path = "tray_windows.rs"]
pub mod tray;
pub mod welcome;
pub mod widgets;
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 calls, each documented
pub mod winsys;
#[cfg(windows)]
pub mod workers_windows;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

static QUITTING: AtomicBool = AtomicBool::new(false);

/// The owner asked to quit (the window's close then really closes).
pub fn quit() {
    QUITTING.store(true, Ordering::SeqCst);
}

pub fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

/// Write an image of the window as PNG.
pub fn save_png(path: &Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    w.write_image_data(image.as_raw())?;
    Ok(())
}
