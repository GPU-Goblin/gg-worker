//! The application's own preferences (its look), kept in `~/.config/gpu-goblin/desk.toml`: the
//! worker's policy is the worker's, this is only how the window looks.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::i18n::Lang;
use crate::theme::Look;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    look: Look,
    /// The language chosen; none: the system's.
    lang: Option<Lang>,
}

fn path() -> Option<PathBuf> {
    Some(crate::dirs::config_dir()?.join("desk.toml"))
}

fn load() -> Prefs {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// The look chosen last (the system's by default).
pub fn look() -> Look {
    load().look
}

fn store(prefs: &Prefs) {
    let Some(p) = path() else { return };
    if let (Some(dir), Ok(text)) = (p.parent(), toml::to_string(prefs)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&p, text);
    }
}

/// Remember the look chosen.
pub fn set_look(look: Look) {
    let mut prefs = load();
    prefs.look = look;
    store(&prefs);
}

/// The language chosen last, if any.
pub fn lang() -> Option<Lang> {
    load().lang
}

/// Remember the language chosen.
pub fn set_lang(lang: Option<Lang>) {
    let mut prefs = load();
    prefs.lang = lang;
    store(&prefs);
}
