//! `gg-desk`: the contributor's desktop application (ADR-0043). It shows what the machine lends
//! and lets its owner pause, resume and set when and how much to lend, through the worker's local
//! control API; on a machine without a worker, it installs one with the signed installer. Without
//! a display (a server, SSH), or given a command, it runs in the terminal (see `console`).

#![deny(unsafe_code)]
// On Windows, a window application: no console flashes at logon or from the Start menu. Started
// from a terminal, it writes into that terminal (`winsys::attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use eframe::egui;
use gg_desk::{app, console, i18n, link, logo, prefs, quit, quitting, theme};

#[derive(Parser)]
#[command(name = "gg-desk", about = "GPU Goblin: lend this computer to research")]
struct Cli {
    /// Directory of the worker's key, policy and control files.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Start in the system tray, the window hidden (as the session starts it).
    #[arg(long)]
    tray: bool,
    /// Save a picture of the window to this PNG file, then quit.
    #[arg(long, hide = true)]
    screenshot: Option<PathBuf>,
    /// Page to open: overview, settings, setup, about or welcome.
    #[arg(long, hide = true)]
    tab: Option<String>,
    /// Look: system, dark or light (instead of the one chosen last).
    #[arg(long, hide = true)]
    look: Option<String>,
    /// Language code (instead of the one chosen last, or the system's).
    #[arg(long, hide = true)]
    lang: Option<String>,
    #[command(subcommand)]
    command: Option<console::Command>,
}

/// A window can open here.
fn has_display() -> bool {
    if cfg!(target_os = "linux") {
        ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
    } else {
        true
    }
}

fn main() -> eframe::Result {
    #[cfg(windows)]
    gg_desk::winsys::attach_console();
    // The removal asked from Windows' list of installed apps.
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("uninstall") {
        uninstall_windows();
    }
    // The installation without the window, as `install.sh` on Linux (scripts, servers, tests).
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("install") {
        install_windows(std::env::args().skip(2).collect());
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut cli = Cli::parse();
    let dir = cli.dir.clone().unwrap_or_else(link::default_dir);
    // No display: the terminal is the application (the live view, or the status for a script).
    if cli.command.is_none() && !has_display() {
        use std::io::IsTerminal;
        if cli.tray {
            eprintln!("gg-desk: no display for the tray; `gg-desk status` shows the workers");
            std::process::exit(1);
        }
        cli.command = Some(if std::io::stdout().is_terminal() {
            console::Command::Console
        } else {
            console::Command::Status { json: false }
        });
    }
    if let Some(cmd) = cli.command.take() {
        std::process::exit(console::run(cmd, &dir));
    }
    // The window speaks the owner's language (the console stays in English, for logs and scripts).
    let lang = cli
        .lang
        .as_deref()
        .and_then(i18n::Lang::from_locale)
        .or_else(prefs::lang)
        .unwrap_or_else(i18n::Lang::system);
    i18n::set(lang);
    let ctx_slot: Arc<std::sync::Mutex<Option<egui::Context>>> = Default::default();
    let repaint = {
        let slot = ctx_slot.clone();
        Arc::new(move || {
            if let Some(c) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                c.request_repaint();
            }
        })
    };
    let link = link::Link::start(dir, repaint);
    // On Windows the application starts the workers (systemd does on Linux).
    #[cfg(windows)]
    if cli.screenshot.is_none() {
        gg_desk::workers_windows::keep_running();
    }
    if cli.screenshot.is_some() {
        // Let the link get a first answer before the picture.
        std::thread::sleep(std::time::Duration::from_millis(2500));
    }
    let look = cli.look.as_deref().map(|l| match l {
        "dark" => theme::Look::Dark,
        "light" => theme::Look::Light,
        _ => theme::Look::System,
    });
    let icon = {
        let p = theme::Palette::DARK;
        egui::IconData {
            rgba: logo::rgba(256, p.accent, p.accent2, None),
            width: 256,
            height: 256,
        }
    };
    let hidden = cli.tray && cli.screenshot.is_none();
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GPU Goblin")
            .with_app_id("gpu-goblin")
            .with_icon(icon)
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size([820.0, 580.0])
            .with_visible(!hidden),
        ..Default::default()
    };
    #[cfg(windows)]
    {
        options.renderer = eframe::Renderer::Wgpu;
    }
    // Only on X11 (XWayland under Wayland) can a window hide into the tray and come back.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() {
        options.event_loop_builder = Some(Box::new(|b| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            b.with_x11();
        }));
    }
    eframe::run_native(
        "GPU Goblin",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(theme::fonts(lang));
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(h) = cc.window_handle()
                    && let RawWindowHandle::Win32(w) = h.as_raw()
                {
                    gg_desk::winsys::set_window_icons(w.hwnd.get());
                }
            }
            *ctx_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(cc.egui_ctx.clone());
            #[cfg(any(target_os = "linux", windows))]
            if cli.screenshot.is_none() && !gg_desk::tray::start(link.clone(), cc.egui_ctx.clone())
            {
                // No tray host: the window is the application, and closing it quits.
                quit();
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            quit();
            let mut app = app::App::new(link, cli.screenshot, cli.tab.as_deref(), look);
            app.start_hidden = hidden && !quitting();
            Ok(Box::new(app))
        }),
    )
}

/// Remove GPU Goblin from this computer, after asking; the worker's key and settings stay unless
/// the owner also wants them gone.
#[cfg(windows)]
fn uninstall_windows() -> ! {
    use gg_desk::i18n::tr;
    use gg_desk::winsys::{ask, tell};
    i18n::set(prefs::lang().unwrap_or_else(i18n::Lang::system));
    if !ask("GPU Goblin", tr("Remove GPU Goblin from this computer?")) {
        std::process::exit(1);
    }
    let everything = ask(
        "GPU Goblin",
        tr(
            "Also delete this computer's worker key and settings? It will then have to be attached to your account again.",
        ),
    );
    match gg_desk::install_windows::uninstall(everything) {
        Ok(()) => {
            tell("GPU Goblin", tr("GPU Goblin was removed."), false);
            std::process::exit(0)
        }
        Err(e) => {
            tell("GPU Goblin", &e, true);
            std::process::exit(1)
        }
    }
}

/// `gg-desk install [--server-url URL] [--gpu | --cuda] [--enroll CODE]`: install the worker from the signed
/// release, printing each step; exit 0 once installed and started.
#[cfg(windows)]
fn install_windows(args: Vec<String>) -> ! {
    use gg_desk::install::Engine;
    let mut plan = gg_desk::install_windows::Plan {
        server_url: gg_desk::account::DEFAULT_SERVER.to_owned(),
        engine: Engine::Cpu,
        enroll: String::new(),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server-url" => plan.server_url = it.next().unwrap_or_default(),
            "--enroll" => plan.enroll = it.next().unwrap_or_default(),
            "--gpu" => plan.engine = Engine::Gpu,
            "--cuda" => plan.engine = Engine::Cuda,
            other => {
                eprintln!("gg-desk install: unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gg-desk install: {e}");
            std::process::exit(1);
        }
    };
    let say = |line: String| println!("{line}");
    match gg_desk::install_windows::run(&plan, rt.handle(), &say) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("\nERROR: {e}\nNothing was started.");
            std::process::exit(1)
        }
    }
}
