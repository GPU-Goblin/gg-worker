//! The window: a side bar with the name and the pages, the state of the machine with its controls,
//! the settings, and the first setup.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use egui_phosphor::regular as icon;
use gg_client::worker_control::{Policy, Waiting, Window};

use crate::account::Account;
use crate::i18n::{Lang, tr, trf};
use crate::link::{Link, Member, Order, Snapshot};
use crate::setup::Setup;
use crate::theme::{self, Look, Palette};
use crate::welcome::{Outcome, Welcome};
use crate::widgets;

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const DAY_LABELS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];
const GIB: f64 = (1u64 << 30) as f64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Settings,
    Setup,
    About,
}

/// What the machine is doing, as the owner sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Computing a task.
    Computing,
    /// Ready: asking the grid for work.
    Lending,
    /// Held back by the owner's settings, or not yet authorized.
    Waiting,
    Paused,
    /// No worker answers.
    Offline,
}

impl Mood {
    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            Mood::Computing | Mood::Lending => p.accent,
            Mood::Waiting => p.warn,
            Mood::Paused => p.idle,
            Mood::Offline => p.danger,
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Mood::Computing => icon::LIGHTNING,
            Mood::Lending => icon::ROCKET_LAUNCH,
            Mood::Waiting => icon::HOURGLASS,
            Mood::Paused => icon::PAUSE,
            Mood::Offline => icon::PLUG,
        }
    }
}

/// The application's state.
pub struct App {
    link: Link,
    tab: Tab,
    /// The policy being edited, and the worker's policy it started from.
    draft: Option<(Policy, Policy)>,
    setup: Setup,
    look: Look,
    /// The palette in force, and the logo drawn in it.
    applied: Option<(Palette, TextureHandle)>,
    /// The window was closed: hidden, the tray keeps the application.
    pub hidden: Arc<Mutex<bool>>,
    /// Save a picture of the window here, then quit (documentation, checks).
    screenshot: Option<std::path::PathBuf>,
    frames: u32,
    /// Open the setup when no worker is installed.
    auto_setup: bool,
    /// Started in the tray: the window hides on its first frame.
    pub start_hidden: bool,
    /// The contributor signed in here.
    account: Option<Account>,
    /// The welcome screen (sign in or create an account), while shown.
    welcome: Option<Welcome>,
    /// The language spoken, and the one the fonts were chosen for.
    lang: Lang,
    fonts_for: Option<Lang>,
}

impl App {
    pub fn new(
        link: Link,
        screenshot: Option<std::path::PathBuf>,
        tab: Option<&str>,
        look: Option<Look>,
    ) -> Self {
        Self {
            link,
            tab: match tab {
                Some("settings") => Tab::Settings,
                Some("setup") => Tab::Setup,
                Some("about") => Tab::About,
                _ => Tab::Overview,
            },
            draft: None,
            setup: Setup::default(),
            look: look.unwrap_or_else(crate::prefs::look),
            applied: None,
            hidden: Arc::new(Mutex::new(false)),
            screenshot,
            frames: 0,
            auto_setup: tab.is_none(),
            start_hidden: false,
            account: crate::account::load(),
            welcome: None,
            lang: crate::i18n::current(),
            fonts_for: None,
        }
        .greeted(tab)
    }
}

impl App {
    /// Nobody signed in: the welcome screen first (a page named for a picture skips it).
    fn greeted(mut self, tab: Option<&str>) -> Self {
        if tab == Some("welcome") || (tab.is_none() && self.account.is_none()) {
            self.welcome = Some(Welcome::default());
        }
        self
    }
}

/// What the owner reads about the state: its mood, a title, a sentence.
pub fn describe(snap: &Snapshot) -> (Mood, String, String) {
    let Some(s) = snap.status.as_ref().filter(|_| snap.connected()) else {
        return (
            Mood::Offline,
            tr("Not connected").into(),
            snap.error
                .clone()
                .unwrap_or_else(|| tr("Looking for this computer's worker…").into()),
        );
    };
    match (s.state.as_str(), &s.waiting) {
        ("computing", _) => (
            Mood::Computing,
            tr("Computing for science").into(),
            s.task
                .as_ref()
                .map(|t| format!("{} · {}", kind_name(&t.kind), elapsed(t.started_ms)))
                .unwrap_or_default(),
        ),
        // Paused as soon as the owner says so (the worker's own state follows at its next check).
        (_, Some(Waiting::Paused)) | (_, _) if s.policy.paused => (
            Mood::Paused,
            tr("Paused").into(),
            tr("Lending is paused. Resume whenever you like.").into(),
        ),
        (_, Some(w)) => (Mood::Waiting, tr("Standing by").into(), reason(w)),
        ("ready", None) => (
            Mood::Lending,
            tr("Ready to lend").into(),
            tr("Waiting for the next task from the grid.").into(),
        ),
        ("waiting_authorization", _) => (
            Mood::Waiting,
            tr("Almost there").into(),
            tr("This computer waits to be attached to your account.").into(),
        ),
        ("starting", _) => (
            Mood::Waiting,
            tr("Starting").into(),
            tr("The worker is getting ready.").into(),
        ),
        (other, _) => (Mood::Waiting, other.to_owned(), String::new()),
    }
}

/// Why the worker waits, in words.
pub fn reason(w: &Waiting) -> String {
    match w {
        Waiting::Paused => tr("Lending is paused. Resume whenever you like.").into(),
        Waiting::OutsideSchedule => tr("Outside the hours you chose.").into(),
        Waiting::NotIdle => tr("You are using the computer: it lends once you step away.").into(),
        Waiting::GpuHot { temp_c, max_c } => trf(
            "Letting the graphics card cool down: {temp} °C, your limit is {max} °C.",
            &[("temp", temp_c), ("max", max_c)],
        ),
        Waiting::GpuInUse { percent } => trf(
            "Another program is using the graphics card ({percent} %). Enjoy your game!",
            &[("percent", percent)],
        ),
    }
}

/// A task's kind, in words.
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        "tensor.matmul.v1" => tr("Matrix multiplication"),
        "mlp.train.v1" => tr("Training a small network"),
        "mlp.grad.v1" | "graph.grad.v1" => tr("Training a model"),
        "graph.stage.v1" => tr("Training a large model (one stage)"),
        "replica.compare.v1" => tr("Checking another contributor's result"),
        _ => tr("A research task"),
    }
}

fn elapsed(started_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(started_ms);
    let s = now.saturating_sub(started_ms) / 1000;
    if s < 60 {
        format!("{s} s")
    } else {
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.start_hidden && self.frames == 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            *self.hidden.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
        // Closing the window keeps the application in the tray.
        if ctx.input(|i| i.viewport().close_requested()) && !crate::quitting() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            *self.hidden.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
        let system_dark = ctx.system_theme().is_none_or(|t| t == egui::Theme::Dark);
        let p = Palette::of(self.look, system_dark);
        if self.applied.as_ref().is_none_or(|(a, _)| *a != p) {
            theme::apply(&ctx, &p);
            let n = 128;
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [n, n],
                &crate::logo::rgba(n as u32, p.accent, p.accent2, None),
            );
            let tex = ctx.load_texture("logo", img, TextureOptions::LINEAR);
            self.applied = Some((p, tex));
        }
        let snap = self.link.snapshot();
        if self.frames == 0
            && self.auto_setup
            && !crate::link::worker_dirs(self.link.base())
                .iter()
                .any(|d| d.join("worker.key").exists() || d.join("control.addr").exists())
        {
            self.tab = Tab::Setup;
        }
        if self.fonts_for != Some(self.lang) {
            ctx.set_fonts(theme::fonts(self.lang));
            self.fonts_for = Some(self.lang);
        }
        if self.welcome.is_some() {
            self.welcome_screen(ui, &p);
            self.frames += 1;
            self.maybe_screenshot(&ctx);
            return;
        }
        self.sidebar(ui, &p);
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(32, 26)),
            )
            .show(ui, |ui| {
                self.header(ui, &p);
                ui.add_space(14.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.tab {
                        Tab::Overview => self.overview(ui, &p, &snap),
                        Tab::Settings => self.settings(ui, &p, &snap),
                        Tab::Setup => self.setup.ui(
                            ui,
                            &p,
                            snap.connected(),
                            self.account.as_ref(),
                            &self.link.handle,
                        ),
                        Tab::About => {
                            crate::about::ui(ui, &p, self.applied.as_ref().map(|(_, t)| t))
                        }
                    });
            });
        self.frames += 1;
        self.maybe_screenshot(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    fn maybe_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = &self.screenshot else {
            return;
        };
        if self.frames == 4 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            if let Err(e) = crate::save_png(path, &image) {
                eprintln!("screenshot: {e}");
            }
            crate::quit();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, p: &Palette) {
        let logo = self.applied.as_ref().map(|(_, t)| t.clone());
        egui::Panel::left("side")
            .exact_size(236.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(p.side)
                    .inner_margin(Margin::symmetric(16, 22))
                    .stroke(Stroke::new(1.0, p.border)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(t) = &logo {
                        ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(46.0)));
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add_space(3.0);
                        ui.label(RichText::new("GPU Goblin").font(theme::font(theme::BOLD, 22.0)));
                        ui.label(
                            RichText::new(tr("GPU for science"))
                                .size(12.5)
                                .color(p.muted),
                        );
                    });
                });
                ui.add_space(26.0);
                for (tab, ic, label) in [
                    (Tab::Overview, icon::HOUSE, tr("Overview")),
                    (Tab::Settings, icon::SLIDERS_HORIZONTAL, tr("Settings")),
                    (Tab::Setup, icon::DOWNLOAD_SIMPLE, tr("Setup")),
                    (Tab::About, icon::HEART, tr("About")),
                ] {
                    if nav(ui, p, self.tab == tab, ic, label) {
                        self.tab = tab;
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(format!("GPU Goblin {}", env!("CARGO_PKG_VERSION")))
                            .size(11.5)
                            .color(p.muted),
                    );
                    ui.add_space(4.0);
                    if let Some(look) = look_switch(ui, p, self.look) {
                        self.look = look;
                        crate::prefs::set_look(look);
                    }
                    ui.label(RichText::new(tr("Appearance")).size(12.0).color(p.muted));
                    ui.add_space(6.0);
                    if let Some(l) = language_menu(ui, self.lang, "side-lang") {
                        self.choose(l);
                    }
                    ui.label(RichText::new(tr("Language")).size(12.0).color(p.muted));
                    ui.add_space(10.0);
                    self.account_chip(ui, p);
                });
            });
    }

    /// Speak `lang` from now on, and remember it.
    fn choose(&mut self, lang: Lang) {
        self.lang = lang;
        crate::i18n::set(lang);
        crate::prefs::set_lang(Some(lang));
    }

    fn welcome_screen(&mut self, ui: &mut Ui, p: &Palette) {
        let logo = self.applied.as_ref().map(|(_, t)| t.clone());
        let rt = self.link.handle.clone();
        let mut outcome = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(32, 22)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(look) = look_switch(ui, p, self.look) {
                            self.look = look;
                            crate::prefs::set_look(look);
                        }
                        if let Some(l) = language_menu(ui, self.lang, "welcome-lang") {
                            self.choose(l);
                        }
                        ui.label(
                            RichText::new(icon::GLOBE)
                                .font(theme::icons(18.0))
                                .color(p.muted),
                        );
                    });
                });
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(w) = self.welcome.as_mut() {
                            outcome = w.ui(ui, p, logo.as_ref(), &rt);
                        }
                    });
            });
        match outcome {
            Some(Outcome::SignedIn(a)) => {
                self.account = Some(a);
                self.welcome = None;
            }
            None => {}
        }
    }

    /// Who is signed in, and signing out; or signing in.
    fn account_chip(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(a) = self.account.clone() else {
            return;
        };
        Frame::new()
            .fill(if p.dark { p.raised } else { p.bg })
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 15.0, p.accent);
                    let initial: String = a
                        .email
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .collect();
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        initial,
                        theme::font(theme::BOLD, 15.0),
                        p.on_accent,
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(icon::SIGN_OUT)
                                        .font(theme::icons(17.0))
                                        .color(p.muted),
                                )
                                .frame(false),
                            )
                            .on_hover_text(tr("Sign out"))
                            .clicked()
                        {
                            let rt = self.link.handle.clone();
                            let gone = a.clone();
                            rt.spawn(async move { crate::account::sign_out(&gone).await });
                            crate::account::forget();
                            self.account = None;
                            self.welcome = Some(Welcome::default());
                        }
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new(tr("Signed in")).size(11.5).color(p.muted));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&a.email).font(theme::font(theme::MEDIUM, 13.0)),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
            });
    }

    fn header(&mut self, ui: &mut Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let title = match self.tab {
                Tab::Overview => tr("Overview"),
                Tab::Settings => tr("Settings"),
                Tab::Setup => tr("Setup"),
                Tab::About => tr("About"),
            };
            ui.label(RichText::new(title).font(theme::font(theme::BOLD, 28.0)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let members = self.link.members();
                let up = members.iter().filter(|m| m.snap.connected()).count();
                let (color, text) = match (up, members.len()) {
                    (0, _) => (p.danger, tr("Worker offline").to_owned()),
                    (_, 0 | 1) => (p.accent, tr("Worker connected").to_owned()),
                    (u, n) if u == n => (p.accent, trf("{n} workers connected", &[("n", &n)])),
                    (u, n) => (
                        p.warn,
                        trf("{u} of {n} workers connected", &[("u", &u), ("n", &n)]),
                    ),
                };
                widgets::pill(
                    ui,
                    p,
                    color,
                    RichText::new(text).font(theme::font(theme::MEDIUM, 12.5)),
                );
            });
        });
    }

    fn overview(&mut self, ui: &mut Ui, p: &Palette, snap: &Snapshot) {
        let (mood, title, detail) = describe(snap);
        let color = mood.color(p);
        let paused = snap.status.as_ref().is_some_and(|s| s.policy.paused);
        widgets::card(ui, p, |ui| {
            ui.horizontal(|ui| {
                widgets::ring(ui, p, color, mood == Mood::Computing, mood.icon());
                ui.add_space(12.0);
                // The words take what the buttons leave.
                let buttons = if paused || !snap.connected() {
                    190.0
                } else {
                    290.0
                };
                let words = (ui.available_width() - buttons).max(160.0);
                ui.allocate_ui(Vec2::new(words, 96.0), |ui| {
                    ui.vertical(|ui| {
                        ui.add_space(14.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(title).font(theme::font(theme::BOLD, 25.0)),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(detail).size(14.5).color(p.muted))
                                .wrap(),
                        );
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !snap.connected() {
                        if widgets::primary(ui, p, icon::DOWNLOAD_SIMPLE, tr("Set up"))
                            .on_hover_text(tr("Install the worker on this computer"))
                            .clicked()
                        {
                            self.tab = Tab::Setup;
                        }
                        return;
                    }
                    {
                        if paused {
                            if widgets::primary(ui, p, icon::PLAY, tr("Resume")).clicked() {
                                self.link.send(Order::Resume);
                            }
                        } else {
                            if widgets::secondary(ui, p, icon::STOP, tr("Stop now"), p.danger)
                                .on_hover_text(tr(
                                    "The task running goes back to the grid at once. No penalty.",
                                ))
                                .clicked()
                            {
                                self.link.send(Order::Pause { now: true });
                            }
                            if widgets::primary(ui, p, icon::PAUSE, tr("Pause"))
                                .on_hover_text(tr("The task running finishes first."))
                                .clicked()
                            {
                                self.link.send(Order::Pause { now: false });
                            }
                        }
                    }
                });
            });
            if mood == Mood::Computing {
                ui.add_space(10.0);
                progress(ui, p);
            }
        });
        ui.add_space(14.0);
        let s = snap.status.as_ref();
        let temp = s.and_then(|s| s.readings.gpu_temp_c);
        let busy = s.and_then(|s| s.readings.gpu_busy_percent);
        let temp_color = match temp {
            Some(t) if t >= 83 => p.danger,
            Some(t) if t >= 72 => p.warn,
            _ => p.accent2,
        };
        let stats = [
            (
                icon::COINS,
                p.accent,
                s.and_then(|s| s.earned.clone())
                    .unwrap_or_else(|| "–".into()),
                tr("Credits earned").to_owned(),
            ),
            (
                icon::CHECK_CIRCLE,
                p.accent2,
                s.and_then(|s| s.accepted)
                    .map_or_else(|| "–".into(), |n| n.to_string()),
                tr("Results accepted").to_owned(),
            ),
            (
                icon::CPU,
                Color32::from_rgb(0x9b, 0x7b, 0xff),
                s.map_or_else(|| "–".into(), |s| s.submitted.to_string()),
                s.map_or_else(
                    || tr("Tasks this session").to_owned(),
                    |s| trf("Tasks this session · {n} returned", &[("n", &s.abandoned)]),
                ),
            ),
            (
                icon::THERMOMETER,
                temp_color,
                match (temp, s.map(|s| s.engine.as_str())) {
                    (Some(t), _) => format!("{t} °C"),
                    (None, Some("cpu")) => tr("Not lent").into(),
                    (None, _) => "–".into(),
                },
                busy.map_or_else(
                    || tr("Graphics card").to_owned(),
                    |b| trf("Graphics card · {b} % busy", &[("b", &b)]),
                ),
            ),
        ];
        let cols = if ui.available_width() > 860.0 { 4 } else { 2 };
        let gap = 14.0;
        let width = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
        for row in stats.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (ic, c, value, name) in row {
                    ui.allocate_ui(Vec2::new(width, 90.0), |ui| {
                        widgets::stat(ui, p, ic, *c, value, name);
                    });
                }
            });
            ui.add_space(gap - 10.0);
        }
        ui.add_space(10.0);
        let members = self.link.members();
        if members.len() > 1 {
            self.cards(ui, p, &members);
            ui.add_space(14.0);
        }
        widgets::card(ui, p, |ui| {
            widgets::card_title(ui, p, icon::DESKTOP, tr("This computer"));
            let session = s.map(|s| match (s.readings.locked, s.readings.idle_secs) {
                (Some(true), _) => tr("Locked").to_owned(),
                (_, Some(0)) => tr("In use").to_owned(),
                (_, Some(n)) => trf("Idle for {n} min", &[("n", &(n / 60))]),
                _ => tr("No desktop session").to_owned(),
            });
            egui::Grid::new("computer")
                .num_columns(2)
                .spacing([28.0, 8.0])
                .show(ui, |ui| {
                    let row = |ui: &mut Ui, k: &str, v: String| {
                        ui.label(RichText::new(k).color(p.muted));
                        ui.label(RichText::new(v).font(theme::font(theme::MEDIUM, 14.5)));
                        ui.end_row();
                    };
                    row(ui, tr("Session"), session.unwrap_or_else(|| "–".into()));
                    row(
                        ui,
                        tr("Computes on"),
                        s.map_or_else(
                            || "–".into(),
                            |s| match s.engine.as_str() {
                                "cpu" => tr("Processor").to_owned(),
                                e if e.starts_with("cuda") => tr("Graphics card (CUDA)").to_owned(),
                                _ => tr("Graphics card").to_owned(),
                            },
                        ),
                    );
                    row(
                        ui,
                        tr("Worker"),
                        s.map_or_else(|| "–".into(), |s| trf("version {v}", &[("v", &s.version)])),
                    );
                });
        });
    }

    /// A machine with several graphics cards: one worker per card, each with its state and its
    /// own pause.
    fn cards(&mut self, ui: &mut Ui, p: &Palette, members: &[Member]) {
        widgets::card(ui, p, |ui| {
            widgets::card_title(
                ui,
                p,
                icon::GRAPHICS_CARD,
                &trf("Graphics cards · {n}", &[("n", &members.len())]),
            );
            for (k, m) in members.iter().enumerate() {
                let (mood, title, _) = describe(&m.snap);
                let s = m.snap.status.as_ref().filter(|_| m.snap.connected());
                if k > 0 {
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 5.0, mood.color(p));
                    ui.label(RichText::new(&m.label).font(theme::font(theme::SEMIBOLD, 14.5)));
                    ui.label(RichText::new(title).color(p.muted));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let Some(s) = s else {
                            return;
                        };
                        let small = |ui: &mut Ui, ic: &str, text: &str, color: Color32| {
                            ui.add(
                                egui::Button::new(theme::labelled(
                                    ic,
                                    text,
                                    theme::font(theme::MEDIUM, 13.0),
                                    color,
                                ))
                                .corner_radius(CornerRadius::same(16)),
                            )
                            .clicked()
                        };
                        if s.policy.paused {
                            if small(ui, icon::PLAY, tr("Resume"), p.accent) {
                                self.link.send_to(k, Order::Resume);
                            }
                        } else if small(ui, icon::PAUSE, tr("Pause"), p.text) {
                            self.link.send_to(k, Order::Pause { now: false });
                        }
                        let facts = [
                            s.readings.gpu_temp_c.map(|t| format!("{t} °C")),
                            s.readings.gpu_busy_percent.map(|b| format!("{b} % busy")),
                            Some(format!("{} tasks", s.submitted)),
                        ];
                        let facts: Vec<String> = facts.into_iter().flatten().collect();
                        ui.label(RichText::new(facts.join("  ·  ")).size(13.0).color(p.muted));
                    });
                });
            }
        });
    }

    fn settings(&mut self, ui: &mut Ui, p: &Palette, snap: &Snapshot) {
        let Some(current) = snap.status.as_ref().map(|s| s.policy.clone()) else {
            widgets::card(ui, p, |ui| {
                widgets::card_title(ui, p, icon::PLUG, tr("Waiting for the worker"));
                ui.label(
                    RichText::new(tr(
                        "The settings belong to this computer's worker: they appear once it runs.",
                    ))
                    .color(p.muted),
                );
            });
            return;
        };
        // Follow the worker's policy, unless the owner is editing it.
        let follow = match &self.draft {
            None => true,
            Some((d, from)) => *from != current && d == from,
        };
        if follow {
            self.draft = Some((current.clone(), current.clone()));
        }
        let Some((d, from)) = self.draft.as_mut() else {
            return;
        };
        widgets::card(ui, p, |ui| {
            widgets::card_title(ui, p, icon::CLOCK, tr("When to lend"));
            widgets::switch(
                ui,
                p,
                &mut d.only_when_idle,
                tr("Only when I'm away"),
                tr("Lends while your session is locked, or after a while without you."),
            );
            if d.only_when_idle {
                ui.horizontal(|ui| {
                    ui.add_space(52.0);
                    let mut minutes = (d.idle_after_secs / 60) as u32;
                    ui.label(RichText::new(tr("Away for at least")).color(p.muted));
                    ui.add(egui::Slider::new(&mut minutes, 1..=120).suffix(" min"));
                    d.idle_after_secs = minutes as u64 * 60;
                });
            }
            ui.add_space(8.0);
            ui.label(RichText::new(tr("Hours")).font(theme::font(theme::MEDIUM, 14.5)));
            ui.label(
                RichText::new(if d.schedule.is_empty() {
                    tr("At any time. Add hours to lend only within them.")
                } else {
                    tr("Only within these hours (your local time). A window may run past midnight.")
                })
                .size(12.5)
                .color(p.muted),
            );
            let mut remove = None;
            for (k, w) in d.schedule.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (day, label) in DAYS.iter().zip(DAY_LABELS) {
                        let mut on = w.days.is_empty() || w.days.iter().any(|x| x == day);
                        if widgets::chip(ui, p, &mut on, label).changed() {
                            set_day(w, day, on);
                        }
                    }
                    ui.add_space(10.0);
                    ui.add(egui::TextEdit::singleline(&mut w.from).desired_width(52.0));
                    ui.label(RichText::new(tr("to")).color(p.muted));
                    ui.add(egui::TextEdit::singleline(&mut w.to).desired_width(52.0));
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(icon::TRASH)
                                    .font(theme::icons(17.0))
                                    .color(p.muted),
                            )
                            .frame(false),
                        )
                        .on_hover_text(tr("Remove these hours"))
                        .clicked()
                    {
                        remove = Some(k);
                    }
                });
            }
            if let Some(k) = remove {
                d.schedule.remove(k);
            }
            if ui
                .button(theme::labelled(
                    icon::PLUS,
                    tr("Add hours"),
                    theme::font(theme::MEDIUM, 14.0),
                    p.text,
                ))
                .clicked()
            {
                d.schedule.push(Window {
                    days: Vec::new(),
                    from: "19:00".into(),
                    to: "08:00".into(),
                });
            }
        });
        ui.add_space(14.0);
        widgets::card(ui, p, |ui| {
            widgets::card_title(ui, p, icon::GRAPHICS_CARD, tr("Graphics card"));
            widgets::switch(
                ui,
                p,
                &mut d.yield_to_other_gpu_apps,
                tr("Give way to games"),
                tr("No new task while a game or another program uses the card."),
            );
            let mut limit = d.gpu_max_temp_c.is_some();
            widgets::switch(
                ui,
                p,
                &mut limit,
                tr("Keep it cool"),
                tr("No new task above this temperature, until the card cools down."),
            );
            let mut t = d.gpu_max_temp_c.unwrap_or(80);
            if limit {
                ui.horizontal(|ui| {
                    ui.add_space(52.0);
                    ui.add(egui::Slider::new(&mut t, 50..=95).suffix(" °C"));
                });
            }
            d.gpu_max_temp_c = limit.then_some(t);
            memory(
                ui,
                p,
                &mut d.device_memory_bytes,
                tr("Limit its memory"),
                tr("Graphics memory a task may use."),
            );
        });
        ui.add_space(14.0);
        widgets::card(ui, p, |ui| {
            widgets::card_title(ui, p, icon::MEMORY, tr("Computer"));
            memory(
                ui,
                p,
                &mut d.max_ram_bytes,
                tr("Limit memory"),
                tr("Memory a task may use (at most what was installed)."),
            );
        });
        ui.add_space(14.0);
        let changed = *d != *from;
        let valid = d.validate();
        if changed {
            let fill = if p.dark {
                Color32::from_rgb(0x0b, 0x0d, 0x11)
            } else {
                Color32::from_rgb(0x11, 0x18, 0x27)
            };
            Frame::new()
                .fill(fill)
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::symmetric(18, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        match &valid {
                            Ok(()) => {
                                ui.label(
                                    RichText::new(tr("You have unsaved changes."))
                                        .color(Color32::WHITE),
                                );
                            }
                            Err(e) => {
                                ui.label(theme::labelled(
                                    icon::WARNING,
                                    e,
                                    egui::FontId::proportional(14.0),
                                    p.warn,
                                ));
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    valid.is_ok(),
                                    egui::Button::new(
                                        RichText::new(tr("Save changes"))
                                            .font(theme::font(theme::SEMIBOLD, 14.0))
                                            .color(p.on_accent),
                                    )
                                    .fill(p.accent)
                                    .corner_radius(CornerRadius::same(18)),
                                )
                                .clicked()
                            {
                                self.link.send(Order::SetPolicy(d.clone()));
                                *from = d.clone();
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(tr("Reset")).color(Color32::WHITE),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                *d = from.clone();
                            }
                        });
                    });
                });
        }
    }
}

/// A page in the side bar. Returns whether it was clicked.
fn nav(ui: &mut Ui, p: &Palette, selected: bool, ic: &str, label: &str) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
    let fill = if selected {
        p.tint(p.accent, if p.dark { 0.16 } else { 0.12 })
    } else if resp.hovered() {
        if p.dark { p.raised } else { p.bg }
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    if selected {
        let bar =
            egui::Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 10.0), Vec2::new(3.0, 22.0));
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    }
    let color = if selected { p.accent } else { p.muted };
    ui.painter().text(
        rect.left_center() + Vec2::new(16.0, 0.0),
        Align2::LEFT_CENTER,
        ic,
        theme::icons(19.0),
        color,
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(46.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        theme::font(
            if selected {
                theme::SEMIBOLD
            } else {
                theme::MEDIUM
            },
            15.0,
        ),
        if selected { p.text } else { p.muted },
    );
    ui.add_space(2.0);
    resp.clicked()
}

/// System, dark or light. Returns the new choice when it changed.
fn look_switch(ui: &mut Ui, p: &Palette, current: Look) -> Option<Look> {
    let mut chosen = None;
    Frame::new()
        .fill(if p.dark { p.raised } else { p.bg })
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (look, ic, tip) in [
                    (Look::System, icon::MONITOR, tr("Like the system")),
                    (Look::Dark, icon::MOON, tr("Dark")),
                    (Look::Light, icon::SUN, tr("Light")),
                ] {
                    let on = current == look;
                    let r = ui
                        .add(
                            egui::Button::new(
                                RichText::new(ic).font(theme::icons(17.0)).color(if on {
                                    p.on_accent
                                } else {
                                    p.muted
                                }),
                            )
                            .fill(if on { p.accent } else { Color32::TRANSPARENT })
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(9))
                            .min_size(Vec2::new(58.0, 30.0)),
                        )
                        .on_hover_text(tip);
                    if r.clicked() && !on {
                        chosen = Some(look);
                    }
                }
            });
        });
    chosen
}

/// The languages, each in its own name. Returns the new choice when it changed.
fn language_menu(ui: &mut Ui, current: Lang, id: &str) -> Option<Lang> {
    let mut chosen = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(current.name())
        .width(170.0)
        .show_ui(ui, |ui| {
            for l in Lang::ALL {
                if ui.selectable_label(l == current, l.name()).clicked() && l != current {
                    chosen = Some(l);
                }
            }
        });
    chosen
}

/// A running bar: the task is being computed.
fn progress(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), p.tint(p.accent, 0.2));
    let t = ui.ctx().input(|i| i.time) as f32;
    let w = rect.width() * 0.28;
    let x = rect.left() + ((t * 0.45).fract() * (rect.width() + w)) - w;
    let bar = egui::Rect::from_min_max(
        egui::pos2(x.max(rect.left()), rect.top()),
        egui::pos2((x + w).min(rect.right()), rect.bottom()),
    );
    if bar.width() > 0.0 {
        ui.painter()
            .rect_filled(bar, CornerRadius::same(3), p.accent);
    }
    ui.ctx().request_repaint();
}

fn set_day(w: &mut Window, day: &str, on: bool) {
    if w.days.is_empty() {
        w.days = DAYS.iter().map(|d| d.to_string()).collect();
    }
    w.days.retain(|d| d != day);
    if on {
        w.days.push(day.to_owned());
    }
    w.days.sort_by_key(|d| DAYS.iter().position(|x| x == d));
    if w.days.len() == 7 {
        w.days.clear();
    }
}

fn memory(ui: &mut Ui, p: &Palette, value: &mut Option<u64>, label: &str, help: &str) {
    let mut on = value.is_some();
    widgets::switch(ui, p, &mut on, label, help);
    let mut gib = value.map_or(4.0, |b| b as f64 / GIB);
    if on {
        ui.horizontal(|ui| {
            ui.add_space(52.0);
            ui.add(
                egui::DragValue::new(&mut gib)
                    .range(0.5..=512.0)
                    .speed(0.25)
                    .suffix(" GiB"),
            );
        });
    }
    *value = on.then_some((gib * GIB) as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_toggle_back_to_every_day() {
        let mut w = Window {
            days: Vec::new(),
            from: "19:00".into(),
            to: "08:00".into(),
        };
        set_day(&mut w, "sun", false);
        assert_eq!(w.days.len(), 6);
        assert!(!w.days.contains(&"sun".to_owned()));
        set_day(&mut w, "sun", true);
        assert!(w.days.is_empty(), "all seven: every day");
        set_day(&mut w, "mon", false);
        set_day(&mut w, "tue", false);
        assert_eq!(w.days, ["wed", "thu", "fri", "sat", "sun"]);
    }

    #[test]
    fn states_and_kinds_are_words() {
        assert!(
            reason(&Waiting::GpuHot {
                temp_c: 83,
                max_c: 80
            })
            .contains("83 °C")
        );
        let (mood, title, detail) = describe(&Snapshot {
            error: Some("the worker is not running (or not installed)".into()),
            ..Default::default()
        });
        assert_eq!(mood, Mood::Offline);
        assert_eq!(title, "Not connected");
        assert!(detail.contains("not running"));
        assert_eq!(kind_name("graph.grad.v1"), "Training a model");
        assert_eq!(kind_name("something.new"), "A research task");
    }
}
