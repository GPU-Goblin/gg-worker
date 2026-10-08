//! The welcome screen: the application is used signed in, so it opens here while nobody is (and
//! after signing out). One form signs in and signs up: an e-mail address, then the code sent to it
//! (no password; the account is created by the first right code). Also the language, and the grid
//! for those who run their own.

use eframe::egui::{self, Align, Layout, RichText, TextureHandle, Vec2};
use egui_phosphor::regular as icon;

use crate::account::{self, Account, Failure, Pending};
use crate::i18n::{tr, trf};
use crate::theme::{self, Palette};
use crate::widgets;

enum Step {
    Email,
    Code { email: String },
}

/// What the owner did on the screen.
pub enum Outcome {
    SignedIn(Account),
}

pub struct Welcome {
    step: Step,
    email: String,
    code: String,
    server: String,
    error: Option<String>,
    note: Option<String>,
    sending: Option<Pending<Result<(), Failure>>>,
    checking: Option<Pending<Result<Account, Failure>>>,
}

impl Default for Welcome {
    fn default() -> Self {
        Self {
            step: Step::Email,
            email: String::new(),
            code: String::new(),
            server: account::server(),
            error: None,
            note: None,
            sending: None,
            checking: None,
        }
    }
}

/// A failure, in the owner's words and language.
pub fn explain(f: &Failure) -> String {
    match f {
        Failure::Unreachable(why) => trf(
            "The grid cannot be reached ({why}). Check your connection.",
            &[("why", why)],
        ),
        Failure::SignedOut => tr("Your session ended: sign in again.").to_owned(),
        Failure::WrongCode => {
            tr("This code is wrong, expired or already used. Ask for a new one.").to_owned()
        }
        Failure::Closed => tr("This grid does not take new contributors yet.").to_owned(),
        Failure::Refused(why) => trf("The grid refused: {why}", &[("why", why)]),
    }
}

fn valid_email(e: &str) -> bool {
    let e = e.trim();
    let Some((user, domain)) = e.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.contains(char::is_whitespace)
        && e.len() <= 254
}

impl Welcome {
    /// Draw the screen; returns what the owner decided, once.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        logo: Option<&TextureHandle>,
        rt: &tokio::runtime::Handle,
    ) -> Option<Outcome> {
        let ctx = ui.ctx().clone();
        let repaint = move || ctx.request_repaint();
        let mut outcome = None;
        if let Some(r) = self.sending.as_ref().and_then(|s| s.take()) {
            self.sending = None;
            match r {
                Ok(()) => {
                    self.step = Step::Code {
                        email: self.email.trim().to_lowercase(),
                    };
                    self.code.clear();
                    self.error = None;
                }
                Err(f) => self.error = Some(explain(&f)),
            }
        }
        if let Some(r) = self.checking.as_ref().and_then(|s| s.take()) {
            self.checking = None;
            match r {
                Ok(a) => {
                    if let Err(e) = account::save(&a) {
                        self.error = Some(trf(
                            "Signed in, but the session could not be kept: {why}",
                            &[("why", &e)],
                        ));
                    }
                    outcome = Some(Outcome::SignedIn(a));
                }
                Err(f) => self.error = Some(explain(&f)),
            }
        }
        let busy = self.sending.is_some() || self.checking.is_some();

        ui.vertical_centered(|ui| {
            ui.set_max_width(460.0);
            ui.add_space((ui.available_height() * 0.08).clamp(12.0, 60.0));
            if let Some(t) = logo {
                ui.add(egui::Image::new(t).fit_to_exact_size(Vec2::splat(84.0)));
            }
            ui.add_space(10.0);
            ui.label(RichText::new("GPU Goblin").font(theme::font(theme::BOLD, 34.0)));
            ui.label(
                RichText::new(tr("Lend your computer's power to science."))
                    .size(16.0)
                    .color(p.muted),
            );
            ui.add_space(22.0);
            widgets::card(ui, p, |ui| {
                ui.with_layout(Layout::top_down(Align::Min), |ui| match &self.step {
                    Step::Email => self.email_step(ui, p, rt, busy, &repaint),
                    Step::Code { email } => {
                        let email = email.clone();
                        self.code_step(ui, p, rt, busy, &email, &repaint)
                    }
                });
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                egui::CollapsingHeader::new(
                    RichText::new(tr("Grid address")).size(12.5).color(p.muted),
                )
                .id_salt("grid")
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server)
                            .desired_width(300.0)
                            .hint_text(account::DEFAULT_SERVER),
                    );
                    ui.label(
                        RichText::new(tr("Change it only if your lab runs its own grid."))
                            .size(12.0)
                            .color(p.muted),
                    );
                });
            });
        });
        outcome
    }

    fn email_step(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        rt: &tokio::runtime::Handle,
        busy: bool,
        repaint: &(impl Fn() + Clone + Send + 'static),
    ) {
        ui.label(RichText::new(tr("Sign in")).font(theme::font(theme::BOLD, 20.0)));
        ui.label(RichText::new(tr("Enter your e-mail. We'll send you a code.")).color(p.muted));
        ui.add_space(8.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.email)
                .hint_text(tr("you@example.org"))
                .font(theme::font(theme::MEDIUM, 16.0))
                .margin(egui::Margin::symmetric(12, 10))
                .desired_width(f32::INFINITY),
        );
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        self.error_line(ui, p);
        ui.add_space(6.0);
        let ok = valid_email(&self.email) && !busy;
        let clicked = ui
            .add_enabled_ui(ok, |ui| {
                widgets::primary(ui, p, icon::ENVELOPE_SIMPLE, tr("Send me a code"))
            })
            .inner
            .clicked();
        if busy {
            ui.spinner();
        }
        if ok && (clicked || enter) {
            match account::check_server(&self.server) {
                Ok(server) => {
                    self.server = server.clone();
                    self.error = None;
                    let email = self.email.trim().to_owned();
                    let r = repaint.clone();
                    self.sending = Some(Pending::spawn(
                        rt,
                        async move { account::send_code(&server, &email).await },
                        r,
                    ));
                }
                Err(_) => {
                    self.error = Some(tr("The grid address must start with https://").to_owned())
                }
            }
        }
    }

    fn code_step(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        rt: &tokio::runtime::Handle,
        busy: bool,
        email: &str,
        repaint: &(impl Fn() + Clone + Send + 'static),
    ) {
        ui.label(RichText::new(tr("Enter your code")).font(theme::font(theme::BOLD, 20.0)));
        ui.label(RichText::new(trf("Sent to {email}.", &[("email", &email)])).color(p.muted));
        if let Some(n) = &self.note {
            ui.label(RichText::new(n).size(12.5).color(p.accent));
        }
        ui.add_space(8.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.code)
                .hint_text("12345678")
                .char_limit(8)
                .font(theme::font(theme::BOLD, 24.0))
                .horizontal_align(Align::Center)
                .margin(egui::Margin::symmetric(12, 10))
                .desired_width(f32::INFINITY),
        );
        self.code.retain(|c| c.is_ascii_digit());
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        self.error_line(ui, p);
        ui.add_space(6.0);
        let ok = self.code.len() == 8 && !busy;
        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled_ui(ok, |ui| {
                    widgets::primary(ui, p, icon::SIGN_IN, tr("Continue"))
                })
                .inner
                .clicked();
            if busy {
                ui.spinner();
            }
            if ok && (clicked || enter) {
                self.error = None;
                let (server, email, code) =
                    (self.server.clone(), email.to_owned(), self.code.clone());
                self.checking = Some(Pending::spawn(
                    rt,
                    async move { account::sign_in(&server, &email, &code).await },
                    repaint.clone(),
                ));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.link(tr("Use another address")).clicked() {
                self.step = Step::Email;
                self.error = None;
                self.note = None;
            }
            ui.label(RichText::new("·").color(p.muted));
            if ui.link(tr("Send a new code")).clicked() && !busy {
                let (server, email) = (self.server.clone(), email.to_owned());
                self.note = Some(tr("A new code is on its way.").to_owned());
                self.sending = Some(Pending::spawn(
                    rt,
                    async move { account::send_code(&server, &email).await },
                    repaint.clone(),
                ));
            }
        });
    }

    fn error_line(&self, ui: &mut egui::Ui, p: &Palette) {
        if let Some(e) = &self.error {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(theme::labelled(
                    icon::WARNING,
                    e,
                    egui::FontId::proportional(13.5),
                    p.danger,
                ))
                .wrap(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_checked_before_anything_is_sent() {
        for good in ["ada@example.org", " Ada.L@lab.univ.fr "] {
            assert!(valid_email(good), "{good}");
        }
        for bad in [
            "",
            "ada",
            "ada@",
            "@x.org",
            "ada@org",
            "a b@x.org",
            "ada@x.org.",
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }
}
