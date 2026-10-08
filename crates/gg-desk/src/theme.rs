//! The look: a dark and a light palette, the Inter typeface, the icons, the shapes.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Visuals,
};
use serde::{Deserialize, Serialize};

/// The owner's choice of look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Look {
    /// As the system says.
    #[default]
    System,
    Dark,
    Light,
}

/// The colours of one look.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background.
    pub bg: Color32,
    /// Side bar.
    pub side: Color32,
    /// Cards.
    pub card: Color32,
    /// Cards under the pointer, inputs.
    pub raised: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// The brand: goblin green.
    pub accent: Color32,
    /// The other end of the brand's gradient.
    pub accent2: Color32,
    /// Text on the accent.
    pub on_accent: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub idle: Color32,
}

impl Palette {
    pub const DARK: Palette = Palette {
        dark: true,
        bg: Color32::from_rgb(0x12, 0x14, 0x19),
        side: Color32::from_rgb(0x0b, 0x0d, 0x11),
        card: Color32::from_rgb(0x1b, 0x1e, 0x26),
        raised: Color32::from_rgb(0x25, 0x29, 0x33),
        border: Color32::from_rgb(0x2c, 0x31, 0x3c),
        text: Color32::from_rgb(0xec, 0xee, 0xf3),
        muted: Color32::from_rgb(0x96, 0x9e, 0xad),
        accent: Color32::from_rgb(0x3d, 0xdc, 0x84),
        accent2: Color32::from_rgb(0x2b, 0xc7, 0xd9),
        on_accent: Color32::from_rgb(0x06, 0x1a, 0x10),
        warn: Color32::from_rgb(0xf5, 0xa6, 0x23),
        danger: Color32::from_rgb(0xf0, 0x55, 0x55),
        idle: Color32::from_rgb(0x6b, 0x72, 0x80),
    };
    pub const LIGHT: Palette = Palette {
        dark: false,
        bg: Color32::from_rgb(0xf3, 0xf4, 0xf7),
        side: Color32::from_rgb(0xff, 0xff, 0xff),
        card: Color32::from_rgb(0xff, 0xff, 0xff),
        raised: Color32::from_rgb(0xee, 0xf0, 0xf4),
        border: Color32::from_rgb(0xe2, 0xe5, 0xeb),
        text: Color32::from_rgb(0x11, 0x18, 0x27),
        muted: Color32::from_rgb(0x64, 0x6b, 0x7a),
        accent: Color32::from_rgb(0x16, 0xa3, 0x4a),
        accent2: Color32::from_rgb(0x08, 0x91, 0xb2),
        on_accent: Color32::WHITE,
        warn: Color32::from_rgb(0xd9, 0x77, 0x06),
        danger: Color32::from_rgb(0xdc, 0x26, 0x26),
        idle: Color32::from_rgb(0x9c, 0xa3, 0xaf),
    };

    /// The palette of `look`, the system being dark or not.
    pub fn of(look: Look, system_dark: bool) -> Palette {
        match look {
            Look::Dark => Self::DARK,
            Look::Light => Self::LIGHT,
            Look::System if system_dark => Self::DARK,
            Look::System => Self::LIGHT,
        }
    }

    /// `c` over the card, at `alpha` (0 – 1): a tinted background.
    pub fn tint(&self, c: Color32, alpha: f32) -> Color32 {
        let mix = |a: u8, b: u8| (a as f32 * alpha + b as f32 * (1.0 - alpha)) as u8;
        Color32::from_rgb(
            mix(c.r(), self.card.r()),
            mix(c.g(), self.card.g()),
            mix(c.b(), self.card.b()),
        )
    }
}

/// Named font families.
pub const MEDIUM: &str = "medium";
pub const SEMIBOLD: &str = "semibold";
pub const BOLD: &str = "bold";
/// The icons alone.
pub const ICONS: &str = "icons";

/// The icon font at `size`.
pub fn icons(size: f32) -> FontId {
    font(ICONS, size)
}

/// An icon then a text, each in its font, as one piece of text for a label or a button.
pub fn labelled(icon: &str, text: &str, font: FontId, color: Color32) -> egui::WidgetText {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |font: FontId| egui::TextFormat {
        font_id: font,
        color,
        valign: egui::Align::Center,
        ..Default::default()
    };
    job.append(icon, 0.0, fmt(icons(font.size + 2.0)));
    job.append(text, 8.0, fmt(font));
    job.into()
}

/// Inter (SIL Open Font License, `assets/fonts/Inter-LICENSE.txt`) for the text, Noto (same
/// licence, `Noto-LICENSE.txt`) for the scripts Inter lacks: Devanagari, Bengali, and the Chinese,
/// Japanese and Korean faces cut down to the characters the translations use (`subset_cjk.py`);
/// the face of the language spoken comes first, as the same character is drawn differently in
/// each. Phosphor for the icons mixed with the text.
pub fn fonts(lang: crate::i18n::Lang) -> FontDefinitions {
    use crate::i18n::Lang;
    let mut f = FontDefinitions::default();
    let add = |f: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        f.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    };
    add(
        &mut f,
        "inter",
        include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    );
    add(
        &mut f,
        "inter-medium",
        include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    );
    add(
        &mut f,
        "inter-semibold",
        include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    );
    add(
        &mut f,
        "inter-bold",
        include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    );
    add(
        &mut f,
        "noto-devanagari",
        include_bytes!("../assets/fonts/NotoSansDevanagari-Regular.ttf"),
    );
    add(
        &mut f,
        "noto-bengali",
        include_bytes!("../assets/fonts/NotoSansBengali-Regular.ttf"),
    );
    add(
        &mut f,
        "noto-cjk-zh",
        include_bytes!("../assets/fonts/NotoSansCJK-zh.otf"),
    );
    add(
        &mut f,
        "noto-cjk-ja",
        include_bytes!("../assets/fonts/NotoSansCJK-ja.otf"),
    );
    add(
        &mut f,
        "noto-cjk-ko",
        include_bytes!("../assets/fonts/NotoSansCJK-ko.otf"),
    );
    let cjk: [&str; 3] = match lang {
        Lang::Ja => ["noto-cjk-ja", "noto-cjk-zh", "noto-cjk-ko"],
        Lang::Ko => ["noto-cjk-ko", "noto-cjk-zh", "noto-cjk-ja"],
        _ => ["noto-cjk-zh", "noto-cjk-ja", "noto-cjk-ko"],
    };
    let scripts: Vec<String> = ["noto-devanagari", "noto-bengali"]
        .into_iter()
        .chain(cjk)
        .map(str::to_owned)
        .collect();
    let defaults = f.families[&FontFamily::Proportional].clone();
    egui_phosphor::add_to_fonts(&mut f, egui_phosphor::Variant::Regular);
    let icons: Vec<String> = f.families[&FontFamily::Proportional]
        .iter()
        .filter(|n| !defaults.contains(n))
        .cloned()
        .collect();
    // Text and icons never share a family: Inter draws some of the private codes the icons use,
    // and the icon font has glyphs for letters. Icons are drawn with `ICONS` (see `labelled`).
    for (family, face) in [
        (FontFamily::Proportional, "inter"),
        (FontFamily::Name(MEDIUM.into()), "inter-medium"),
        (FontFamily::Name(SEMIBOLD.into()), "inter-semibold"),
        (FontFamily::Name(BOLD.into()), "inter-bold"),
    ] {
        let mut list = vec![face.to_owned()];
        list.extend(scripts.iter().cloned());
        list.extend(defaults.iter().cloned());
        f.families.insert(family, list);
    }
    let mut list = icons;
    list.extend(defaults.iter().cloned());
    f.families.insert(FontFamily::Name(ICONS.into()), list);
    f
}

/// A font of `family` at `size`.
pub fn font(family: &str, size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(family.into()))
}

/// Apply `p` to the whole interface.
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.extreme_bg_color = p.raised;
    v.faint_bg_color = p.raised;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.tint(p.accent, 0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 110 } else { 40 }),
    };
    let r = CornerRadius::same(10);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.tint(p.accent, 0.18);
    w.hovered.weak_bg_fill = p.tint(p.accent, 0.12);
    w.hovered.bg_stroke = Stroke::new(1.0, p.tint(p.accent, 0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.tint(p.accent, 0.3);
    w.active.weak_bg_fill = p.tint(p.accent, 0.25);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(2.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open = w.active;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 10.0);
        s.spacing.button_padding = egui::vec2(14.0, 8.0);
        s.spacing.interact_size.y = 34.0;
        s.spacing.slider_width = 200.0;
        s.spacing.window_margin = Margin::same(16);
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.5)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
            (TextStyle::Button, font(MEDIUM, 14.5)),
            (TextStyle::Heading, font(BOLD, 22.0)),
        ]
        .into();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_follows_the_choice_then_the_system() {
        assert!(Palette::of(Look::Dark, false).dark);
        assert!(!Palette::of(Look::Light, true).dark);
        assert!(Palette::of(Look::System, true).dark);
        assert!(!Palette::of(Look::System, false).dark);
    }

    #[test]
    fn a_tint_lies_between_the_colour_and_the_card() {
        let p = Palette::DARK;
        let t = p.tint(p.accent, 0.5);
        assert!(t.g() > p.card.g() && t.g() < p.accent.g());
        assert_eq!(p.tint(p.accent, 1.0), p.accent);
    }
}
