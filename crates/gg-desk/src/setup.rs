//! First setup: attach this machine to the contributor's account and install the worker, with the
//! signed installer of the release. Signed in, the application asks the grid for the enrollment
//! code itself; otherwise the owner types the one their account page shows. The installer is that (`install.sh`, which checks every download against the release
//! key it carries, ADR-0020 and deploy/worker/INSTALL.md). The application adds no check of its
//! own: it runs the installer and shows what it says.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};
use egui_phosphor::regular as icon;

use crate::account::{Account, Failure, Pending};
use crate::i18n::{tr, trf};
use crate::theme::{self, Palette};
use crate::widgets;

/// What the installer printed, and how it ended.
#[derive(Default)]
struct Run {
    log: String,
    ended: Option<bool>,
}

/// The setup screen.
pub struct Setup {
    code: String,
    engine: usize,
    installer: String,
    run: Option<Arc<Mutex<Run>>>,
    /// The enrollment code being asked of the grid, and for which engine.
    fetching: Option<(Pending<Result<String, Failure>>, usize)>,
    error: Option<String>,
    /// Signed in, the owner still prefers to type a code.
    manual: bool,
}

impl Default for Setup {
    fn default() -> Self {
        // The installer shipped with the application, next to it; on Windows the application is
        // the installer (`install_windows`).
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| {
                if cfg!(windows) {
                    Some(e)
                } else {
                    e.parent().map(|d| d.join("install.sh"))
                }
            })
            .filter(|p| p.is_file());
        Self {
            code: String::new(),
            engine: 0,
            installer: beside.map(|p| p.display().to_string()).unwrap_or_default(),
            run: None,
            fetching: None,
            error: None,
            manual: false,
        }
    }
}

/// What to lend: icon, title, line, installer flag.
fn engines() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    let all = vec![
        (icon::CPU, tr("Processor"), tr("Any computer. Slower."), ""),
        (
            icon::GRAPHICS_CARD,
            tr("Graphics card"),
            tr("NVIDIA, AMD or Intel, through Vulkan."),
            "--gpu",
        ),
        (
            icon::LIGHTNING,
            tr("NVIDIA, fastest"),
            tr("Native CUDA engine. 4 GB download."),
            "--cuda",
        ),
    ];
    all
}

/// The installer's arguments for an enrollment code and an engine choice. The code is checked
/// for its shape: it is given to a program, not to a shell.
pub fn arguments(
    installer: &str,
    code: &str,
    engine: usize,
    server: Option<&str>,
) -> Result<Vec<String>, String> {
    let code = code.trim();
    if !code.starts_with("gge_")
        || code.len() > 200
        || !code[4..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(tr("an enrollment code starts with gge_ (your account page gives it)").into());
    }
    let mut args = vec![installer.to_owned(), "--enroll".into(), code.to_owned()];
    if let Some((_, _, _, flag)) = engines().get(engine).filter(|e| !e.3.is_empty()) {
        args.push((*flag).to_owned());
    }
    if let Some(url) = server.filter(|u| *u != crate::account::DEFAULT_SERVER) {
        args.push("--server-url".into());
        args.push(url.to_owned());
    }
    Ok(args)
}

impl Setup {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        connected: bool,
        account: Option<&Account>,
        rt: &tokio::runtime::Handle,
    ) {
        // The code the grid gave: the installer runs with it at once.
        if let Some((pending, engine)) = &self.fetching
            && let Some(r) = pending.take()
        {
            let engine = *engine;
            self.fetching = None;
            match r {
                Ok(code) => match arguments(
                    &self.installer,
                    &code,
                    engine,
                    account.map(|a| a.server.as_str()),
                ) {
                    Ok(args) => self.run = Some(start(args, rt)),
                    Err(e) => self.error = Some(e),
                },
                Err(f) => self.error = Some(crate::welcome::explain(&f)),
            }
        }
        let automatic = account.is_some() && !self.manual;
        widgets::card(ui, p, |ui| {
            if connected {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(icon::SHIELD_CHECK)
                            .font(theme::icons(26.0))
                            .color(p.accent),
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(tr("This computer is set up"))
                                .font(theme::font(theme::BOLD, 18.0)),
                        );
                        ui.label(
                            RichText::new(tr(
                                "Its worker runs. To lend it from another account, install again with that account's code.",
                            ))
                            .color(p.muted),
                        );
                    });
                });
                ui.add_space(8.0);
            }
            ui.label(
                RichText::new(tr("Lend this computer to research"))
                    .font(theme::font(theme::BOLD, 20.0)),
            );
            ui.add_space(6.0);
            let first = if automatic {
                tr("You are signed in: this computer will be attached to your account.")
            } else {
                tr("Sign in, or create your contributor account: it takes an e-mail address.")
            };
            for (n, text) in [
                first,
                tr("Check the installer: its release key must have the fingerprint your operator gave you."),
                tr("Choose what to lend, then install. The worker starts at once."),
            ]
            .iter()
            .enumerate()
            {
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(26.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(r.center(), 13.0, p.tint(p.accent, 0.2));
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        (n + 1).to_string(),
                        theme::font(theme::BOLD, 13.0),
                        p.accent,
                    );
                    ui.add(egui::Label::new(RichText::new(*text).color(p.muted)).wrap());
                });
            }
        });
        ui.add_space(14.0);
        if automatic {
            widgets::card(ui, p, |ui| {
                widgets::card_title(ui, p, icon::USER_CIRCLE, tr("Your account"));
                ui.label(trf(
                    "Signed in as {email}. The enrollment code is asked of the grid when you install.",
                    &[("email", &account.map(|a| a.email.as_str()).unwrap_or_default())],
                ));
                if ui
                    .link(RichText::new(tr("I have an enrollment code")).size(12.5))
                    .clicked()
                {
                    self.manual = true;
                }
            });
        } else {
            widgets::card(ui, p, |ui| {
                widgets::card_title(ui, p, icon::KEY, tr("Your enrollment code"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.code)
                        .password(true)
                        .hint_text("gge_…")
                        .font(theme::font(theme::MEDIUM, 16.0))
                        .margin(egui::Margin::symmetric(12, 10))
                        .desired_width(f32::INFINITY),
                );
                ui.label(
                RichText::new(tr(
                    "Shown once on your account page. It attaches this computer to your account.",
                ))
                .size(12.5)
                .color(p.muted),
            );
            });
        }
        ui.add_space(14.0);
        widgets::card(ui, p, |ui| {
            widgets::card_title(ui, p, icon::CPU, tr("What to lend"));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                for (k, (ic, title, sub, _)) in engines().iter().enumerate() {
                    if option(ui, p, self.engine == k, ic, title, sub) {
                        self.engine = k;
                    }
                }
            });
            ui.add_space(6.0);
            #[cfg(not(windows))]
            egui::CollapsingHeader::new(RichText::new(tr("Installer")).color(p.muted))
                .default_open(self.installer.is_empty())
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.installer)
                            .hint_text(tr("path to install.sh"))
                            .desired_width(f32::INFINITY),
                    );
                });
        });
        ui.add_space(14.0);
        let busy = self.fetching.is_some()
            || self
                .run
                .as_ref()
                .is_some_and(|r| r.lock().unwrap_or_else(|e| e.into_inner()).ended.is_none());
        let server = account.map(|a| a.server.as_str());
        let args = if automatic {
            // The code comes later, from the grid: only its shape matters here.
            arguments(&self.installer, "gge_pending", self.engine, server)
        } else {
            arguments(&self.installer, &self.code, self.engine, server)
        };
        ui.horizontal(|ui| {
            let ready = args.is_ok() && PathBuf::from(&self.installer).is_file() && !busy;
            if ui
                .add_enabled_ui(ready, |ui| {
                    widgets::primary(ui, p, icon::DOWNLOAD_SIMPLE, tr("Install and start"))
                })
                .inner
                .clicked()
            {
                self.error = None;
                if let (true, Some(a)) = (automatic, account) {
                    let a = a.clone();
                    let ctx = ui.ctx().clone();
                    self.fetching = Some((
                        Pending::spawn(
                            rt,
                            async move { crate::account::enrollment(&a).await },
                            move || ctx.request_repaint(),
                        ),
                        self.engine,
                    ));
                } else if let Ok(args) = args.clone() {
                    self.run = Some(start(args, rt));
                }
            }
            if self.fetching.is_some() {
                ui.spinner();
                ui.label(
                    RichText::new(tr("Asking the grid for this computer's code…")).color(p.muted),
                );
            }
            if let Some(e) = &self.error {
                ui.label(theme::labelled(
                    icon::WARNING,
                    e,
                    egui::FontId::proportional(14.0),
                    p.danger,
                ));
            } else if let Err(e) = &args
                && !self.code.is_empty()
            {
                ui.label(theme::labelled(
                    icon::WARNING,
                    e,
                    egui::FontId::proportional(14.0),
                    p.warn,
                ));
            } else if !self.installer.is_empty() && !PathBuf::from(&self.installer).is_file() {
                ui.label(theme::labelled(
                    icon::WARNING,
                    tr("no installer at this path"),
                    egui::FontId::proportional(14.0),
                    p.warn,
                ));
            }
        });
        if let Some(run) = &self.run {
            let r = run.lock().unwrap_or_else(|e| e.into_inner());
            ui.add_space(14.0);
            widgets::card(ui, p, |ui| {
                match r.ended {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(tr("Installing…"))
                                    .font(theme::font(theme::SEMIBOLD, 15.0)),
                            );
                        });
                    }
                    Some(true) => {
                        ui.label(theme::labelled(
                            icon::CHECK_CIRCLE,
                            tr("Installed: the worker starts and appears on your account page."),
                            theme::font(theme::SEMIBOLD, 15.0),
                            p.accent,
                        ));
                    }
                    Some(false) => {
                        ui.label(theme::labelled(
                            icon::WARNING,
                            tr("The installer stopped: nothing was started. Its message is below."),
                            theme::font(theme::SEMIBOLD, 15.0),
                            p.danger,
                        ));
                    }
                }
                egui::Frame::new()
                    .fill(if p.dark { p.side } else { p.raised })
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut r.log.as_str())
                                        .font(egui::TextStyle::Monospace)
                                        .frame(egui::Frame::NONE)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                    });
            });
        }
    }
}

/// A choice as a card: its icon, its title, a line. Returns whether it was clicked.
fn option(ui: &mut egui::Ui, p: &Palette, on: bool, ic: &str, title: &str, sub: &str) -> bool {
    let width = ((ui.available_width() - 24.0) / 3.0).max(170.0) - 1.0;
    egui::Frame::new()
        .fill(if on { p.tint(p.accent, 0.12) } else { p.raised })
        .stroke(egui::Stroke::new(
            if on { 2.0 } else { 1.0 },
            if on { p.accent } else { p.border },
        ))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(width - 32.0);
            ui.set_min_height(92.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(ic).font(theme::icons(24.0)).color(if on {
                    p.accent
                } else {
                    p.muted
                }));
                ui.label(RichText::new(title).font(theme::font(theme::SEMIBOLD, 15.0)));
                ui.add(egui::Label::new(RichText::new(sub).size(12.0).color(p.muted)).wrap());
            });
        })
        .response
        .interact(egui::Sense::click())
        .clicked()
}

/// Install on Windows in the background (the application is the installer), collecting what it
/// says. `args` are those of `install.sh`: the code, the engine flag, the grid's address.
#[cfg(windows)]
fn start(args: Vec<String>, rt: &tokio::runtime::Handle) -> Arc<Mutex<Run>> {
    use crate::install::Engine;
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let plan = crate::install_windows::Plan {
        server_url: value("--server-url")
            .unwrap_or_else(|| crate::account::DEFAULT_SERVER.to_owned()),
        engine: if args.iter().any(|a| a == "--cuda") {
            Engine::Cuda
        } else if args.iter().any(|a| a == "--gpu") {
            Engine::Gpu
        } else {
            Engine::Cpu
        },
        enroll: value("--enroll").unwrap_or_default(),
    };
    let run = Arc::new(Mutex::new(Run::default()));
    let shared = run.clone();
    let rt = rt.clone();
    std::thread::spawn(move || {
        let say = |line: String| {
            let mut r = shared.lock().unwrap_or_else(|e| e.into_inner());
            r.log.push_str(&line);
            r.log.push('\n');
        };
        let result = crate::install_windows::run(&plan, &rt, &say);
        if let Err(e) = &result {
            say(format!("\nERROR: {e}\nNothing was started."));
        }
        shared.lock().unwrap_or_else(|e| e.into_inner()).ended = Some(result.is_ok());
    });
    run
}

/// Run `bash <args>` in the background, collecting its output.
#[cfg(not(windows))]
fn start(args: Vec<String>, _rt: &tokio::runtime::Handle) -> Arc<Mutex<Run>> {
    use std::io::BufRead;
    let run = Arc::new(Mutex::new(Run::default()));
    let shared = run.clone();
    std::thread::spawn(move || {
        let child = std::process::Command::new("bash")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let mut r = shared.lock().unwrap_or_else(|e| e.into_inner());
                r.log = trf("cannot run bash: {why}", &[("why", &e)]);
                r.ended = Some(false);
                return;
            }
        };
        let err = child.stderr.take().map(|e| {
            let s = shared.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(e).lines().map_while(Result::ok) {
                    let mut r = s.lock().unwrap_or_else(|e| e.into_inner());
                    r.log.push_str(&line);
                    r.log.push('\n');
                }
            })
        });
        if let Some(out) = child.stdout.take() {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let mut r = shared.lock().unwrap_or_else(|e| e.into_inner());
                r.log.push_str(&line);
                r.log.push('\n');
            }
        }
        if let Some(t) = err {
            let _ = t.join();
        }
        let ok = child.wait().is_ok_and(|s| s.success());
        shared.lock().unwrap_or_else(|e| e.into_inner()).ended = Some(ok);
    });
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_checked_before_the_installer_runs() {
        assert!(arguments("/x/install.sh", "hello", 0, None).is_err());
        assert!(arguments("/x/install.sh", "gge_abc; rm -rf ~", 0, None).is_err());
        assert_eq!(
            arguments("/x/install.sh", " gge_Ab-9_z ", 1, None).unwrap(),
            ["/x/install.sh", "--enroll", "gge_Ab-9_z", "--gpu"]
        );
        assert_eq!(
            arguments(
                "/x/install.sh",
                "gge_x",
                0,
                Some(crate::account::DEFAULT_SERVER)
            )
            .unwrap(),
            ["/x/install.sh", "--enroll", "gge_x"]
        );
        assert_eq!(
            arguments("/x/install.sh", "gge_x", 2, Some("https://lab.example")).unwrap(),
            [
                "/x/install.sh",
                "--enroll",
                "gge_x",
                "--cuda",
                "--server-url",
                "https://lab.example"
            ]
        );
    }
}
