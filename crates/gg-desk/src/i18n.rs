//! The application's languages. Every text the owner reads goes through [`tr`] (or [`trf`] with
//! `{name}` placeholders), keyed by its English wording; each language is a file of
//! `"English" = "translation"` lines (`i18n/<code>.toml`), checked by the tests: every key used in
//! the code is translated in every language, nothing else is, the placeholders match, and every
//! character has a glyph in the fonts shipped.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// A language, by its ISO 639-1 code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Zh,
    Hi,
    Es,
    Fr,
    Bn,
    Pt,
    Ru,
    Id,
    De,
    Ja,
    Ko,
    Tr,
    Vi,
    It,
}

impl Lang {
    /// Every language, in the order of the menu (by number of speakers, English first).
    pub const ALL: [Lang; 15] = [
        Lang::En,
        Lang::Zh,
        Lang::Hi,
        Lang::Es,
        Lang::Fr,
        Lang::Bn,
        Lang::Pt,
        Lang::Ru,
        Lang::Id,
        Lang::De,
        Lang::Ja,
        Lang::Tr,
        Lang::Vi,
        Lang::Ko,
        Lang::It,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
            Lang::Hi => "hi",
            Lang::Es => "es",
            Lang::Fr => "fr",
            Lang::Bn => "bn",
            Lang::Pt => "pt",
            Lang::Ru => "ru",
            Lang::Id => "id",
            Lang::De => "de",
            Lang::Ja => "ja",
            Lang::Ko => "ko",
            Lang::Tr => "tr",
            Lang::Vi => "vi",
            Lang::It => "it",
        }
    }

    /// Its name, in itself.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Zh => "中文",
            Lang::Hi => "हिन्दी",
            Lang::Es => "Español",
            Lang::Fr => "Français",
            Lang::Bn => "বাংলা",
            Lang::Pt => "Português",
            Lang::Ru => "Русский",
            Lang::Id => "Bahasa Indonesia",
            Lang::De => "Deutsch",
            Lang::Ja => "日本語",
            Lang::Ko => "한국어",
            Lang::Tr => "Türkçe",
            Lang::Vi => "Tiếng Việt",
            Lang::It => "Italiano",
        }
    }

    /// The language of a locale such as `fr_FR.UTF-8` or `pt-BR`.
    pub fn from_locale(locale: &str) -> Option<Lang> {
        let code = locale
            .split(['_', '-', '.', '@'])
            .next()?
            .to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == code)
    }

    /// The system's language (the user's locale on Windows; `LC_ALL`, `LC_MESSAGES`, `LANG`
    /// elsewhere), English otherwise.
    pub fn system() -> Lang {
        #[cfg(windows)]
        if let Some(l) = crate::winsys::user_locale().and_then(|v| Lang::from_locale(&v)) {
            return l;
        }
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
            .and_then(|v| Lang::from_locale(&v))
            .unwrap_or_default()
    }

    /// The translation file, English having none (its keys are its texts).
    fn source(self) -> &'static str {
        match self {
            Lang::En => "",
            Lang::Zh => include_str!("../i18n/zh.toml"),
            Lang::Hi => include_str!("../i18n/hi.toml"),
            Lang::Es => include_str!("../i18n/es.toml"),
            Lang::Fr => include_str!("../i18n/fr.toml"),
            Lang::Bn => include_str!("../i18n/bn.toml"),
            Lang::Pt => include_str!("../i18n/pt.toml"),
            Lang::Ru => include_str!("../i18n/ru.toml"),
            Lang::Id => include_str!("../i18n/id.toml"),
            Lang::De => include_str!("../i18n/de.toml"),
            Lang::Ja => include_str!("../i18n/ja.toml"),
            Lang::Ko => include_str!("../i18n/ko.toml"),
            Lang::Tr => include_str!("../i18n/tr.toml"),
            Lang::Vi => include_str!("../i18n/vi.toml"),
            Lang::It => include_str!("../i18n/it.toml"),
        }
    }
}

type Table = HashMap<String, String>;

fn tables() -> &'static Vec<Table> {
    static TABLES: OnceLock<Vec<Table>> = OnceLock::new();
    TABLES.get_or_init(|| {
        Lang::ALL
            .iter()
            .map(|l| parse(l.source()).unwrap_or_default())
            .collect()
    })
}

/// A translation file's table.
pub fn parse(source: &str) -> Result<Table, String> {
    if source.is_empty() {
        return Ok(Table::new());
    }
    toml::from_str(source).map_err(|e| e.to_string())
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// The language in force.
pub fn current() -> Lang {
    Lang::ALL[CURRENT.load(Ordering::Relaxed)]
}

/// Speak `lang` from now on.
pub fn set(lang: Lang) {
    let k = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0);
    CURRENT.store(k, Ordering::Relaxed);
}

/// `english` in `lang` (English when a translation is missing).
pub fn tr_in(lang: Lang, english: &'static str) -> &'static str {
    let k = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0);
    if k == 0 {
        return english;
    }
    tables()[k]
        .get(english)
        .map(|s| s.as_str())
        .unwrap_or(english)
}

/// `english` in the language in force.
pub fn tr(english: &'static str) -> &'static str {
    tr_in(current(), english)
}

/// [`tr_in`], then each `{name}` replaced by its value.
pub fn trf_in(
    lang: Lang,
    english: &'static str,
    values: &[(&str, &dyn std::fmt::Display)],
) -> String {
    let mut out = tr_in(lang, english).to_owned();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

/// [`tr`], then each `{name}` replaced by its value.
pub fn trf(english: &'static str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
    trf_in(current(), english, values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every `tr("…")` and `trf("…", …)` in the sources.
    fn keys_in_code() -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            // This file holds no text for the owner (and its scanner's own words).
            if path.ends_with("i18n.rs") {
                continue;
            }
            let text = std::fs::read_to_string(path).unwrap();
            for opener in ["tr(", "trf("] {
                let mut rest = text.as_str();
                while let Some(at) = rest.find(opener) {
                    // Not part of a longer name (`xtr(`), nor this test's own text.
                    let before = rest[..at].chars().last();
                    rest = &rest[at + opener.len()..];
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    // The formatter may put the text on the next line.
                    rest = rest.trim_start();
                    let Some(r) = rest.strip_prefix('"') else {
                        continue;
                    };
                    rest = r;
                    let end = rest.find('"').unwrap();
                    let key = &rest[..end];
                    // `\` continues a line in a Rust string: the key is what it reads as.
                    assert!(!key.contains('\\'), "keep keys on one line: {key}");
                    keys.insert(key.to_owned());
                }
            }
        }
        keys.remove("…");
        keys
    }

    fn placeholders(s: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = s;
        while let Some(a) = rest.find('{') {
            rest = &rest[a + 1..];
            if let Some(b) = rest.find('}') {
                out.insert(rest[..b].to_owned());
                rest = &rest[b + 1..];
            }
        }
        out
    }

    #[test]
    fn every_language_translates_every_text_and_nothing_else() {
        let keys = keys_in_code();
        assert!(keys.len() > 50, "found only {} keys", keys.len());
        for lang in Lang::ALL.into_iter().skip(1) {
            let table = parse(lang.source()).unwrap_or_else(|e| panic!("{}: {e}", lang.code()));
            let have: BTreeSet<String> = table.keys().cloned().collect();
            let missing: Vec<&String> = keys.difference(&have).collect();
            let extra: Vec<&String> = have.difference(&keys).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{}: missing {missing:#?}\nnot used {extra:#?}",
                lang.code()
            );
            for (k, v) in &table {
                assert!(!v.trim().is_empty(), "{}: empty for {k}", lang.code());
                assert_eq!(
                    placeholders(k),
                    placeholders(v),
                    "{}: placeholders of {k:?} in {v:?}",
                    lang.code()
                );
            }
        }
    }

    #[test]
    fn every_character_of_every_language_has_a_glyph() {
        let ctx = eframe::egui::Context::default();
        for lang in Lang::ALL {
            ctx.set_fonts(crate::theme::fonts(lang));
            let mut out = ctx.run_ui(Default::default(), |_| {});
            out.textures_delta.clear();
            let table = parse(lang.source()).unwrap();
            let texts: Vec<&str> = table
                .values()
                .map(|s| s.as_str())
                .chain(Lang::ALL.iter().map(|l| l.name()))
                .collect();
            for family in [
                eframe::egui::FontFamily::Proportional,
                eframe::egui::FontFamily::Name(crate::theme::BOLD.into()),
            ] {
                let font = eframe::egui::FontId::new(14.0, family);
                ctx.fonts_mut(|f| {
                    // The check can fail: a character no font here has.
                    assert!(!f.has_glyph(&font, '龘'), "the CJK faces are not cut down");
                    for t in &texts {
                        for c in t.chars().filter(|c| !c.is_whitespace()) {
                            assert!(
                                f.has_glyph(&font, c),
                                "{}: no glyph for {c:?} (U+{:04X}) in {t:?}",
                                lang.code(),
                                c as u32
                            );
                        }
                    }
                });
            }
        }
    }

    #[test]
    fn locales_name_their_language() {
        assert_eq!(Lang::from_locale("fr_FR.UTF-8"), Some(Lang::Fr));
        assert_eq!(Lang::from_locale("pt-BR"), Some(Lang::Pt));
        assert_eq!(Lang::from_locale("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::from_locale("xx_YY"), None);
    }

    #[test]
    fn a_translation_replaces_its_placeholders() {
        assert_eq!(
            trf_in(Lang::En, "{n} workers connected", &[("n", &3)]),
            "3 workers connected"
        );
        assert!(trf_in(Lang::Fr, "{n} workers connected", &[("n", &3)]).contains('3'));
        assert_ne!(tr_in(Lang::Fr, "Overview"), "Overview");
    }
}
