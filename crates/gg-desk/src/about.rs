//! The credits: who made the application, how to reach them, who it thanks, its licence.

use eframe::egui::{self, RichText, TextureHandle, Ui, Vec2};
use egui_phosphor::regular as icon;

use crate::i18n::{tr, trf};
use crate::theme::{self, Palette};
use crate::widgets;

/// Who designed and developed the application.
pub const AUTHOR: &str = "Nicolas Michel";
/// The author's laboratory.
pub const LAB: &str = "LAAS-CNRS";
pub const LAB_URL: &str = "https://www.laas.fr";
/// The author's professional profile.
pub const LINKEDIN: &str = "https://www.linkedin.com/in/nicolas-michel-3161532b4/";
/// Where to write about the application.
pub const CONTACT: &str = "contact@gpugoblin.com";
pub const SITE: &str = "https://gpugoblin.com";

/// The open-source work the grid is built on, and what each does for it.
pub fn thanks() -> [(&'static str, &'static str); 10] {
    [
        ("Rust", tr("the language every part is written in")),
        ("egui, eframe", tr("this window")),
        ("Tokio, reqwest, Axum", tr("the network, on both ends")),
        ("Burn, wgpu", tr("computing on any graphics card")),
        (
            "PyTorch, LibTorch",
            tr("computing with CUDA, and the scientists' models"),
        ),
        ("PostgreSQL, SQLx", tr("the grid's books")),
        (
            "bubblewrap, Landlock, seccomp",
            tr("keeping tasks in their box"),
        ),
        ("ksni", tr("the tray icon")),
        ("Inter, Noto", tr("the typefaces (SIL Open Font License)")),
        ("Phosphor", tr("the icons (MIT)")),
    ]
}

/// The page.
pub fn ui(ui: &mut Ui, p: &Palette, logo: Option<&TextureHandle>) {
    widgets::card(ui, p, |ui| {
        ui.horizontal(|ui| {
            if let Some(t) = logo {
                ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(72.0)));
            }
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add_space(6.0);
                ui.label(RichText::new("GPU Goblin").font(theme::font(theme::BOLD, 26.0)));
                ui.label(
                    RichText::new(trf(
                        "Version {v} · computing power for science, lent by everyone",
                        &[("v", &env!("CARGO_PKG_VERSION"))],
                    ))
                    .color(p.muted),
                );
            });
        });
    });
    ui.add_space(14.0);
    widgets::card(ui, p, |ui| {
        widgets::card_title(ui, p, icon::CODE, tr("Made by"));
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Designed and developed by"));
            ui.label(RichText::new(AUTHOR).font(theme::font(theme::SEMIBOLD, 14.5)));
            ui.label("—");
            ui.hyperlink_to(
                RichText::new(LAB)
                    .font(theme::font(theme::MEDIUM, 14.5))
                    .color(p.accent),
                LAB_URL,
            );
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(icon::ENVELOPE_SIMPLE)
                    .font(theme::icons(17.0))
                    .color(p.accent),
            );
            ui.hyperlink_to(CONTACT, format!("mailto:{CONTACT}"));
            ui.add_space(18.0);
            ui.label(
                RichText::new(icon::GLOBE)
                    .font(theme::icons(17.0))
                    .color(p.accent),
            );
            ui.hyperlink_to("gpugoblin.com", SITE);
            ui.add_space(18.0);
            ui.label(
                RichText::new(icon::LINKEDIN_LOGO)
                    .font(theme::icons(17.0))
                    .color(p.accent),
            );
            ui.hyperlink_to("LinkedIn", LINKEDIN)
                .on_hover_text(format!("{AUTHOR} · LinkedIn"));
        });
        ui.label(
            RichText::new(tr(
                "Questions, a problem, an idea: write, a person answers.",
            ))
            .size(13.0)
            .color(p.muted),
        );
    });
    ui.add_space(14.0);
    widgets::card(ui, p, |ui| {
        widgets::card_title(ui, p, icon::HEART, tr("Thanks"));
        ui.label(tr(
            "To everyone who lends their computer, to the scientists who trust the grid with their work, and to those who tested it first.",
        ));
        ui.add_space(6.0);
        ui.label(RichText::new(tr("Built on open-source work:")).color(p.muted));
        egui::Grid::new("thanks")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                for (name, what) in thanks() {
                    ui.label(RichText::new(name).font(theme::font(theme::MEDIUM, 14.0)));
                    ui.label(RichText::new(what).size(13.5).color(p.muted));
                    ui.end_row();
                }
            });
    });
    ui.add_space(14.0);
    widgets::card(ui, p, |ui| {
        widgets::card_title(ui, p, icon::SCALES, tr("Licence"));
        ui.label(tr(
            "PolyForm Noncommercial 1.0.0: free for research and any noncommercial use; the source is available. Commercial use needs the author's agreement.",
        ));
        ui.hyperlink_to(
            tr("Read the licence"),
            "https://polyformproject.org/licenses/noncommercial/1.0.0/",
        );
    });
}
