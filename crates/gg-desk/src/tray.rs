//! The tray icon (Linux, StatusNotifierItem over D-Bus): the GPU Goblin mark with the state's dot,
//! the state in its tooltip and at the top of its menu, and pause, resume, open, quit without
//! opening the window. The application always starts it; started with the session, the
//! application lives there until opened.

use eframe::egui;
use ksni::TrayMethods;

use crate::app::{Mood, describe};
use crate::i18n::tr;
use crate::link::{Link, Order};
use crate::theme::Palette;

struct Tray {
    link: Link,
    ctx: egui::Context,
    mood: Mood,
    title: String,
    detail: String,
    paused: bool,
}

fn icons(mood: Mood) -> Vec<ksni::Icon> {
    let p = Palette::DARK;
    [22u32, 32, 48, 64]
        .into_iter()
        .map(|n| ksni::Icon {
            width: n as i32,
            height: n as i32,
            data: crate::logo::argb(n, p.accent, p.accent2, Some(mood.color(&p))),
        })
        .collect()
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "gpu-goblin".into()
    }
    fn title(&self) -> String {
        "GPU Goblin".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icons(self.mood)
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: format!("GPU Goblin · {}", self.title),
            description: self.detail.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        show(&self.ctx);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let item = |label: &str, icon: &str, f: fn(&mut Self)| {
            StandardItem {
                label: label.into(),
                icon_name: icon.into(),
                activate: Box::new(f),
                ..Default::default()
            }
            .into()
        };
        let mut items = vec![
            StandardItem {
                label: format!("GPU Goblin · {}", self.title),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            item(tr("Open GPU Goblin"), "window-new", |t| show(&t.ctx)),
        ];
        if self.paused {
            items.push(item(tr("Resume lending"), "media-playback-start", |t| {
                t.link.send(Order::Resume)
            }));
        } else {
            items.push(item(
                tr("Pause (after the task)"),
                "media-playback-pause",
                |t| t.link.send(Order::Pause { now: false }),
            ));
            items.push(item(tr("Stop now"), "media-playback-stop", |t| {
                t.link.send(Order::Pause { now: true })
            }));
        }
        items.push(ksni::MenuItem::Separator);
        items.push(item(tr("Quit GPU Goblin"), "application-exit", |t| {
            crate::quit();
            show(&t.ctx);
            t.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }));
        items
    }
}

fn show(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Start the tray on the link's runtime; it follows the link's state. `false` when there is no
/// tray host (the application then quits when its window closes).
pub fn start(link: Link, ctx: egui::Context) -> bool {
    let handle = link.handle.clone();
    let (mood, title, detail) = describe(&link.snapshot());
    let tray = Tray {
        link: link.clone(),
        ctx,
        mood,
        title,
        detail,
        paused: false,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn(async move {
        match tray.spawn().await {
            Ok(h) => {
                let _ = tx.send(true);
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let snap = link.snapshot();
                    let (mood, title, detail) = describe(&snap);
                    let paused = snap.status.as_ref().is_some_and(|s| s.policy.paused);
                    h.update(|t| {
                        t.mood = mood;
                        t.title = title;
                        t.detail = detail;
                        t.paused = paused;
                    })
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "no tray: the window is the application");
                let _ = tx.send(false);
            }
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or(false)
}
