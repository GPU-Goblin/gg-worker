//! The tray icon on Windows (the notification area): the GPU Goblin mark with the state's dot, the
//! state in its tooltip and at the top of its menu, and pause, resume, open, quit without opening
//! the window, as on Linux. It lives on its own thread, which pumps that thread's Win32 messages:
//! it keeps following the worker while the window is hidden.

use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::app::{Mood, describe};
use crate::i18n::tr;
use crate::link::{Link, Order};
use crate::theme::Palette;

fn icon(mood: Mood) -> Option<Icon> {
    let p = Palette::DARK;
    // The notification area draws 16 or 20 pixels at 100 %, 32 at 200 %: Windows scales down.
    let n = 32;
    Icon::from_rgba(
        crate::logo::rgba(n, p.accent, p.accent2, Some(mood.color(&p))),
        n,
        n,
    )
    .ok()
}

fn show(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// Start the tray on its own thread; it follows the link's state. `false` when it could not be
/// created (the application then quits when its window closes).
pub fn start(link: Link, ctx: egui::Context) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || run(link, ctx, tx));
    if spawned.is_err() {
        return false;
    }
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
}

fn run(link: Link, ctx: egui::Context, ready: std::sync::mpsc::Sender<bool>) {
    let header = MenuItem::new("GPU Goblin", false, None);
    let open = MenuItem::new(tr("Open GPU Goblin"), true, None);
    let pause = MenuItem::new(tr("Pause (after the task)"), true, None);
    let stop = MenuItem::new(tr("Stop now"), true, None);
    let resume = MenuItem::new(tr("Resume lending"), false, None);
    let quit = MenuItem::new(tr("Quit GPU Goblin"), true, None);
    let menu = Menu::new();
    let built = menu.append_items(&[
        &header,
        &PredefinedMenuItem::separator(),
        &open,
        &pause,
        &stop,
        &resume,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);
    let (mood, title, _) = describe(&link.snapshot());
    let tray = built.map_err(|e| e.to_string()).and_then(|()| {
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(format!("GPU Goblin · {title}"))
            .with_icon(icon(mood).ok_or("no icon")?)
            .build()
            .map_err(|e| e.to_string())
    });
    let tray = match tray {
        Ok(t) => {
            let _ = ready.send(true);
            t
        }
        Err(e) => {
            tracing::warn!(error = %e, "no tray: the window is the application");
            let _ = ready.send(false);
            return;
        }
    };
    let mut shown = (mood, String::new(), None::<bool>);
    let mut next = Instant::now();
    loop {
        crate::winsys::pump_messages(Duration::from_millis(250));
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == open.id() {
                show(&ctx);
            } else if e.id == pause.id() {
                link.send(Order::Pause { now: false });
            } else if e.id == stop.id() {
                link.send(Order::Pause { now: true });
            } else if e.id == resume.id() {
                link.send(Order::Resume);
            } else if e.id == quit.id() {
                crate::quit();
                show(&ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show(&ctx);
            }
        }
        if crate::quitting() {
            return;
        }
        if Instant::now() >= next {
            next = Instant::now() + Duration::from_secs(2);
            let snap = link.snapshot();
            let (mood, title, detail) = describe(&snap);
            let paused = snap.status.as_ref().map(|s| s.policy.paused);
            if mood != shown.0 {
                let _ = tray.set_icon(icon(mood));
            }
            if title != shown.1 {
                header.set_text(format!("GPU Goblin · {title}"));
            }
            // The tooltip holds at most 127 characters.
            let tip: String = format!("GPU Goblin · {title}\n{detail}")
                .chars()
                .take(127)
                .collect();
            let _ = tray.set_tooltip(Some(tip));
            if paused != shown.2 {
                let p = paused.unwrap_or(false);
                pause.set_enabled(!p);
                stop.set_enabled(!p);
                resume.set_enabled(p);
            }
            shown = (mood, title, paused);
        }
    }
}
