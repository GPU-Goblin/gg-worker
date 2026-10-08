//! The pieces the window is made of: cards, switches, pill buttons, day chips, the state's ring.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Margin, Response, RichText, Sense, Shadow, Stroke,
    Ui, Vec2, WidgetText,
};

use crate::theme::{self, Palette};

/// A card: rounded, bordered, a soft shadow in the light look.
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(16))
        .inner_margin(Margin::same(20))
        .shadow(if p.dark {
            Shadow::NONE
        } else {
            Shadow {
                offset: [0, 2],
                blur: 10,
                spread: 0,
                color: Color32::from_black_alpha(14),
            }
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A card's title, with its icon.
pub fn card_title(ui: &mut Ui, p: &Palette, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).font(theme::icons(18.0)).color(p.accent));
        ui.label(RichText::new(title).font(theme::font(theme::SEMIBOLD, 16.0)));
    });
    ui.add_space(4.0);
}

/// An on/off switch with its label and, below, what it means.
pub fn switch(ui: &mut Ui, p: &Palette, on: &mut bool, label: &str, help: &str) -> Response {
    let r = ui
        .horizontal(|ui| {
            let size = Vec2::new(42.0, 24.0);
            let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
            if resp.clicked() {
                *on = !*on;
                resp.mark_changed();
            }
            let t = ui.ctx().animate_bool(resp.id, *on);
            let track = if *on {
                p.accent
            } else if p.dark {
                p.raised
            } else {
                p.border
            };
            ui.painter()
                .rect_filled(rect, CornerRadius::same(12), track);
            let x = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
            ui.painter()
                .circle_filled(egui::pos2(x, rect.center().y), 9.0, Color32::WHITE);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(label).font(theme::font(theme::MEDIUM, 14.5)));
                if !help.is_empty() {
                    ui.label(RichText::new(help).size(12.5).color(p.muted));
                }
            });
            resp
        })
        .inner;
    ui.add_space(4.0);
    r
}

/// The main action: a filled pill in the brand's colour.
pub fn primary(ui: &mut Ui, p: &Palette, icon: &str, text: &str) -> Response {
    ui.add(
        egui::Button::new(theme::labelled(
            icon,
            text,
            theme::font(theme::SEMIBOLD, 15.0),
            p.on_accent,
        ))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(22))
        .min_size(Vec2::new(128.0, 44.0)),
    )
}

/// A second action: an outlined pill.
pub fn secondary(ui: &mut Ui, p: &Palette, icon: &str, text: &str, color: Color32) -> Response {
    let hover = p.tint(color, 0.12);
    ui.scope(|ui| {
        ui.visuals_mut().widgets.hovered.weak_bg_fill = hover;
        ui.visuals_mut().widgets.hovered.bg_fill = hover;
        ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::new(1.5, color);
        ui.add(
            egui::Button::new(theme::labelled(
                icon,
                text,
                theme::font(theme::SEMIBOLD, 15.0),
                color,
            ))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.5, color))
            .corner_radius(CornerRadius::same(22))
            .min_size(Vec2::new(128.0, 44.0)),
        )
    })
    .inner
}

/// A small round chip that toggles (a day of the week).
pub fn chip(ui: &mut Ui, p: &Palette, on: &mut bool, text: &str) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let (fill, fg) = if *on {
        (p.accent, p.on_accent)
    } else if resp.hovered() {
        (p.tint(p.accent, 0.15), p.text)
    } else {
        (p.raised, p.muted)
    };
    ui.painter().circle_filled(rect.center(), 16.0, fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        theme::font(theme::SEMIBOLD, 12.5),
        fg,
    );
    resp
}

/// A statistic in a card: its icon on a tinted disc, its value, its name.
pub fn stat(ui: &mut Ui, p: &Palette, icon: &str, color: Color32, value: &str, name: &str) {
    card(ui, p, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 20.0, p.tint(color, 0.18));
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                icon,
                theme::icons(20.0),
                color,
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(value).font(theme::font(theme::BOLD, 21.0)));
                ui.label(RichText::new(name).size(12.5).color(p.muted));
            });
        });
    });
}

/// The state's ring: full and breathing when lending, turning while computing, dim otherwise.
pub fn ring(ui: &mut Ui, p: &Palette, color: Color32, spinning: bool, icon: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
    let c = rect.center();
    let t = ui.ctx().input(|i| i.time) as f32;
    let painter = ui.painter();
    painter.circle_filled(c, 44.0, p.tint(color, 0.12));
    painter.circle_stroke(c, 40.0, Stroke::new(6.0, p.tint(color, 0.28)));
    if spinning {
        let start = t * 3.2;
        let points: Vec<egui::Pos2> = (0..=48)
            .map(|k| {
                let a = start + k as f32 / 48.0 * std::f32::consts::PI * 1.2;
                c + 40.0 * Vec2::angled(a)
            })
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(6.0, color)));
        ui.ctx().request_repaint();
    } else {
        painter.circle_stroke(c, 40.0, Stroke::new(6.0, color));
    }
    painter.text(c, Align2::CENTER_CENTER, icon, theme::icons(34.0), color);
}

/// A little rounded label: a dot and a word (the connection's state).
pub fn pill(ui: &mut Ui, p: &Palette, color: Color32, text: impl Into<WidgetText>) {
    Frame::new()
        .fill(p.tint(color, 0.14))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, color);
                ui.label(text);
            });
        });
}
