//! The console mode: the same application without a window, for a server or a session over SSH.
//! One-shot commands (`status`, `pause`, `resume`, `set`) and a live view (`console`) that answers
//! keys. It talks to the same local control API as the window, to every worker of the machine.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use gg_client::worker_control::{Policy, Window};

use crate::app::{Mood, describe};
use crate::link::{self, Member, Order, Snapshot};

const GIB: u64 = 1 << 30;

#[derive(Subcommand, Debug)]
pub enum Command {
    /// What each worker of this machine is doing.
    Status {
        /// As JSON, for scripts and monitoring.
        #[arg(long)]
        json: bool,
    },
    /// Stop lending (the task running finishes first, unless --now).
    Pause {
        /// Give the task running back to the grid at once (no penalty).
        #[arg(long)]
        now: bool,
        /// Only this graphics card (as `status` numbers them).
        #[arg(long)]
        gpu: Option<usize>,
    },
    /// Lend again.
    Resume {
        /// Only this graphics card (as `status` numbers them).
        #[arg(long)]
        gpu: Option<usize>,
    },
    /// Change when and how much to lend (every worker of the machine, unless --gpu).
    Set(SetArgs),
    /// A live view in the terminal: p pause, s stop now, r resume, q quit.
    Console,
}

#[derive(Args, Debug, Default)]
pub struct SetArgs {
    /// Lend only while nobody uses the machine: on or off.
    #[arg(long, value_parser = on_off)]
    pub only_when_idle: Option<bool>,
    /// Minutes without the owner before lending (with --only-when-idle on).
    #[arg(long)]
    pub idle_minutes: Option<u64>,
    /// When to lend: `always`, or days and hours such as `mon-fri 08:00-18:00`, `sat,sun
    /// 00:00-24:00`, `22:00-07:00` (repeat for several windows).
    #[arg(long)]
    pub schedule: Vec<String>,
    /// Claim nothing while the card is hotter than this, in °C, or `off`.
    #[arg(long, value_parser = number_or_off)]
    pub max_temp: Option<Limit>,
    /// Step aside while another program uses the card: on or off.
    #[arg(long, value_parser = on_off)]
    pub yield_to_apps: Option<bool>,
    /// Host memory lent to a task, GiB, or `off` (the configuration's limit).
    #[arg(long, value_parser = number_or_off)]
    pub max_ram_gib: Option<Limit>,
    /// Graphics memory lent to a task, GiB, or `off` (the configuration's limit).
    #[arg(long, value_parser = number_or_off)]
    pub gpu_memory_gib: Option<Limit>,
    /// Only this graphics card (as `status` numbers them).
    #[arg(long)]
    pub gpu: Option<usize>,
}

fn on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" | "yes" | "true" => Ok(true),
        "off" | "no" | "false" => Ok(false),
        _ => Err(format!("`{s}`: on or off")),
    }
}

/// A limit, or `off` (none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit(pub Option<u64>);

fn number_or_off(s: &str) -> Result<Limit, String> {
    if s == "off" {
        return Ok(Limit(None));
    }
    s.parse()
        .map(|n| Limit(Some(n)))
        .map_err(|_| format!("`{s}`: a whole number or off"))
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn hhmm(s: &str) -> Result<String, String> {
    let (h, m) = s.split_once(':').ok_or(format!("`{s}`: HH:MM"))?;
    let (h, m): (u32, u32) = (
        h.parse().map_err(|_| format!("`{s}`: HH:MM"))?,
        m.parse().map_err(|_| format!("`{s}`: HH:MM"))?,
    );
    if h > 24 || m > 59 || (h == 24 && m != 0) {
        return Err(format!("`{s}`: no such time"));
    }
    Ok(format!("{h:02}:{m:02}"))
}

/// `mon-fri 08:00-18:00`, `sat,sun 00:00-24:00`, `22:00-07:00` (every day).
pub fn parse_window(s: &str) -> Result<Window, String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let (days, hours) = match parts.as_slice() {
        [hours] => (None, *hours),
        [days, hours] => (Some(*days), *hours),
        _ => return Err(format!("`{s}`: [days] HH:MM-HH:MM")),
    };
    let mut out = Vec::new();
    for part in days.map(|d| d.split(',').collect()).unwrap_or(Vec::new()) {
        let at = |d: &str| {
            DAYS.iter().position(|x| *x == d).ok_or(format!(
                "`{d}`: a day is mon, tue, wed, thu, fri, sat or sun"
            ))
        };
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (at(a)?, at(b)?);
                let mut k = a;
                loop {
                    out.push(DAYS[k].to_owned());
                    if k == b {
                        break;
                    }
                    k = (k + 1) % 7;
                }
            }
            None => out.push(DAYS[at(part)?].to_owned()),
        }
    }
    out.dedup();
    if out.len() == 7 {
        out.clear();
    }
    let (from, to) = hours
        .split_once('-')
        .ok_or(format!("`{hours}`: HH:MM-HH:MM"))?;
    Ok(Window {
        days: out,
        from: hhmm(from)?,
        to: hhmm(to)?,
    })
}

/// `policy` changed as `set` asks.
pub fn apply(mut policy: Policy, a: &SetArgs) -> Result<Policy, String> {
    if let Some(v) = a.only_when_idle {
        policy.only_when_idle = v;
    }
    if let Some(m) = a.idle_minutes {
        policy.idle_after_secs = m.saturating_mul(60);
    }
    if a.schedule.iter().any(|s| s == "always") {
        if a.schedule.len() > 1 {
            return Err("`always` stands alone".into());
        }
        policy.schedule.clear();
    } else if !a.schedule.is_empty() {
        policy.schedule = a
            .schedule
            .iter()
            .map(|s| parse_window(s))
            .collect::<Result<_, _>>()?;
    }
    if let Some(Limit(t)) = a.max_temp {
        policy.gpu_max_temp_c = t.map(|t| t.min(u32::MAX as u64) as u32);
    }
    if let Some(v) = a.yield_to_apps {
        policy.yield_to_other_gpu_apps = v;
    }
    if let Some(Limit(r)) = a.max_ram_gib {
        policy.max_ram_bytes = r.map(|g| g.saturating_mul(GIB));
    }
    if let Some(Limit(r)) = a.gpu_memory_gib {
        policy.device_memory_bytes = r.map(|g| g.saturating_mul(GIB));
    }
    policy.validate()?;
    Ok(policy)
}

/// One line on the policy.
pub fn policy_line(p: &Policy) -> String {
    let mut out = Vec::new();
    if p.paused {
        out.push("paused".to_owned());
    }
    if p.only_when_idle {
        out.push(format!("only when idle ({} min)", p.idle_after_secs / 60));
    }
    if p.schedule.is_empty() {
        out.push("any time".to_owned());
    } else {
        let w: Vec<String> = p
            .schedule
            .iter()
            .map(|w| {
                let days = if w.days.is_empty() {
                    "every day".to_owned()
                } else {
                    w.days.join(",")
                };
                format!("{days} {}-{}", w.from, w.to)
            })
            .collect();
        out.push(w.join("; "));
    }
    if let Some(t) = p.gpu_max_temp_c {
        out.push(format!("up to {t} °C"));
    }
    if p.yield_to_other_gpu_apps {
        out.push("steps aside for other programs".to_owned());
    }
    if let Some(b) = p.max_ram_bytes {
        out.push(format!("{:.0} GiB RAM", b as f64 / GIB as f64));
    }
    if let Some(b) = p.device_memory_bytes {
        out.push(format!("{:.0} GiB graphics memory", b as f64 / GIB as f64));
    }
    out.join(" · ")
}

/// Run a command; the exit code (0: done, 1: failed, 2: no worker answers).
pub fn run(cmd: Command, base: &Path) -> i32 {
    match execute(cmd, base) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gg-desk: {e:#}");
            1
        }
    }
}

fn runtime() -> anyhow::Result<(tokio::runtime::Runtime, reqwest::Client)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    Ok((rt, http))
}

/// Ask the workers, as the window's link does.
fn gather(
    rt: &tokio::runtime::Runtime,
    http: &reqwest::Client,
    base: &Path,
    order: Option<(Option<usize>, Order)>,
) -> Vec<(PathBuf, Snapshot)> {
    rt.block_on(link::ask(http, base, order))
        .into_iter()
        .map(|(d, r)| {
            let snap = match r {
                Ok(s) => Snapshot {
                    status: Some(s),
                    error: None,
                    answered: Some(Instant::now()),
                },
                Err(e) => Snapshot {
                    error: Some(e),
                    ..Default::default()
                },
            };
            (d, snap)
        })
        .collect()
}

/// The place in [`link::worker_dirs`] of the worker on card `gpu`.
fn place(snaps: &[(PathBuf, Snapshot)], gpu: usize) -> anyhow::Result<usize> {
    let members = link::members_of(snaps);
    members
        .iter()
        .position(|m| m.label == format!("GPU {gpu}"))
        .with_context(|| {
            let known: Vec<&str> = members.iter().map(|m| m.label.as_str()).collect();
            format!(
                "no worker on GPU {gpu} (this machine has: {})",
                known.join(", ")
            )
        })
}

fn execute(cmd: Command, base: &Path) -> anyhow::Result<i32> {
    let (rt, http) = runtime()?;
    let order = |gpu: Option<usize>, o: Order| -> anyhow::Result<(Option<usize>, Order)> {
        let only = match gpu {
            Some(g) => Some(place(&gather(&rt, &http, base, None), g)?),
            None => None,
        };
        Ok((only, o))
    };
    let snaps = match cmd {
        Command::Status { json } => {
            let snaps = gather(&rt, &http, base, None);
            if json {
                println!("{}", serde_json::to_string_pretty(&as_json(&snaps))?);
            } else {
                print!("{}", render(base, &snaps, false));
            }
            snaps
        }
        Command::Pause { now, gpu } => {
            let o = order(gpu, Order::Pause { now })?;
            let snaps = gather(&rt, &http, base, Some(o));
            print!("{}", render(base, &snaps, false));
            snaps
        }
        Command::Resume { gpu } => {
            let o = order(gpu, Order::Resume)?;
            let snaps = gather(&rt, &http, base, Some(o));
            print!("{}", render(base, &snaps, false));
            snaps
        }
        Command::Set(args) => {
            let now = gather(&rt, &http, base, None);
            let only = args.gpu.map(|g| place(&now, g)).transpose()?;
            let from = now
                .iter()
                .enumerate()
                .filter(|(k, _)| only.is_none_or(|o| o == *k))
                .find_map(|(_, (_, s))| s.status.as_ref().map(|s| s.policy.clone()))
                .context("no worker answers: nothing to change")?;
            let policy = apply(from, &args).map_err(anyhow::Error::msg)?;
            let snaps = gather(&rt, &http, base, Some((only, Order::SetPolicy(policy))));
            print!("{}", render(base, &snaps, false));
            snaps
        }
        Command::Console => {
            live(&rt, &http, base)?;
            return Ok(0);
        }
    };
    let errors: Vec<&str> = snaps
        .iter()
        .filter_map(|(_, s)| s.error.as_deref())
        .collect();
    Ok(if snaps.iter().all(|(_, s)| s.status.is_none()) {
        2
    } else if errors.is_empty() {
        0
    } else {
        1
    })
}

fn as_json(snaps: &[(PathBuf, Snapshot)]) -> serde_json::Value {
    let members = link::members_of(snaps);
    serde_json::Value::Array(
        snaps
            .iter()
            .zip(members)
            .map(|((d, s), m)| {
                serde_json::json!({
                    "worker": m.label,
                    "dir": d,
                    "status": s.status,
                    "error": s.error,
                })
            })
            .collect(),
    )
}

fn mood_word(m: Mood) -> &'static str {
    match m {
        Mood::Computing => "COMPUTING",
        Mood::Lending => "READY",
        Mood::Waiting => "WAITING",
        Mood::Paused => "PAUSED",
        Mood::Offline => "OFFLINE",
    }
}

fn mood_color(m: Mood) -> &'static str {
    match m {
        Mood::Computing | Mood::Lending => "\x1b[32m",
        Mood::Waiting => "\x1b[33m",
        Mood::Paused => "\x1b[90m",
        Mood::Offline => "\x1b[31m",
    }
}

/// The machine as text; `color` for a terminal.
pub fn render(base: &Path, snaps: &[(PathBuf, Snapshot)], color: bool) -> String {
    use std::fmt::Write as _;
    let (bold, dim, reset) = if color {
        ("\x1b[1m", "\x1b[90m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let members: Vec<Member> = link::members_of(snaps);
    let whole = link::fold(&members);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{bold}GPU Goblin{reset} {} · {} worker{} · {dim}{}{reset}",
        env!("CARGO_PKG_VERSION"),
        members.len(),
        if members.len() == 1 { "" } else { "s" },
        base.display()
    );
    let _ = writeln!(out);
    for m in &members {
        let (mood, title, detail) = describe(&m.snap);
        let tint = if color { mood_color(mood) } else { "" };
        let _ = writeln!(
            out,
            "  {bold}{:<7}{reset} {tint}{:<10}{reset} {title}",
            m.label,
            mood_word(mood)
        );
        let mut facts = Vec::new();
        if let Some(s) = m.snap.status.as_ref().filter(|_| m.snap.connected()) {
            facts.push(s.engine.clone());
            if let Some(t) = s.readings.gpu_temp_c {
                facts.push(format!("{t} °C"));
            }
            if let Some(b) = s.readings.gpu_busy_percent {
                facts.push(format!("{b} % busy"));
            }
            facts.push(format!(
                "{} task{} this session",
                s.submitted,
                if s.submitted == 1 { "" } else { "s" }
            ));
        }
        if !detail.is_empty() {
            let _ = writeln!(out, "          {dim}{detail}{reset}");
        }
        if !facts.is_empty() {
            let _ = writeln!(out, "          {dim}{}{reset}", facts.join(" · "));
        }
    }
    if let Some(s) = whole.status.as_ref().filter(|_| whole.connected()) {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  Credits earned {bold}{}{reset} TC · results accepted {bold}{}{reset}",
            s.earned.as_deref().unwrap_or("–"),
            s.accepted.map_or_else(|| "–".to_owned(), |n| n.to_string())
        );
        let _ = writeln!(out, "  Lends: {}", policy_line(&s.policy));
    }
    out
}

/// Put the terminal back as it was, whatever happens.
struct Screen;

impl Screen {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        Ok(Self)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn live(rt: &tokio::runtime::Runtime, http: &reqwest::Client, base: &Path) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        bail!("the live view needs a terminal; use `gg-desk status` in scripts");
    }
    let _screen = Screen::enter()?;
    let mut order = None;
    let mut note = String::new();
    loop {
        let snaps = gather(rt, http, base, order.take());
        let mut frame = render(base, &snaps, true);
        frame.push('\n');
        if !note.is_empty() {
            frame.push_str(&format!("  {note}\n\n"));
        }
        frame.push_str(
            "  \x1b[1mp\x1b[0m pause  \x1b[1ms\x1b[0m stop now  \x1b[1mr\x1b[0m resume  \
             \x1b[1mq\x1b[0m quit\n",
        );
        let mut out = std::io::stdout();
        crossterm::queue!(
            out,
            crossterm::cursor::MoveTo(0, 0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        // Raw mode: a line ends with a carriage return too.
        out.write_all(frame.replace('\n', "\r\n").as_bytes())?;
        out.flush()?;
        let until = Instant::now() + Duration::from_secs(2);
        while order.is_none() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || !event::poll(left)? {
                break;
            }
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char('p') => {
                        order = Some((None, Order::Pause { now: false }));
                        note = "Pausing: the tasks running finish first.".into();
                    }
                    KeyCode::Char('s') => {
                        order = Some((None, Order::Pause { now: true }));
                        note = "Stopped: the tasks running went back to the grid.".into();
                    }
                    KeyCode::Char('r') => {
                        order = Some((None, Order::Resume));
                        note = "Lending again.".into();
                    }
                    _ => {}
                },
                Event::Resize(..) => break,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_read_as_people_write_them() {
        let w = parse_window("mon-fri 08:00-18:00").unwrap();
        assert_eq!(w.days, ["mon", "tue", "wed", "thu", "fri"]);
        assert_eq!((w.from.as_str(), w.to.as_str()), ("08:00", "18:00"));
        let w = parse_window("sat,sun 0:00-24:00").unwrap();
        assert_eq!(w.days, ["sat", "sun"]);
        assert_eq!(w.from, "00:00");
        let w = parse_window("fri-mon 22:00-7:30").unwrap();
        assert_eq!(w.days, ["fri", "sat", "sun", "mon"]);
        assert_eq!(w.to, "07:30");
        assert!(parse_window("22:00-07:00").unwrap().days.is_empty());
        assert!(parse_window("mon-sun 22:00-07:00").unwrap().days.is_empty());
        for bad in [
            "",
            "mon",
            "funday 08:00-09:00",
            "08:00",
            "25:00-26:00",
            "a b c",
        ] {
            assert!(parse_window(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_changes_only_what_it_names_and_refuses_nonsense() {
        let base = Policy::default();
        let a = SetArgs {
            only_when_idle: Some(true),
            idle_minutes: Some(15),
            schedule: vec!["mon-fri 18:00-08:00".into()],
            max_temp: Some(Limit(Some(80))),
            gpu_memory_gib: Some(Limit(Some(6))),
            ..Default::default()
        };
        let p = apply(base.clone(), &a).unwrap();
        assert!(p.only_when_idle);
        assert_eq!(p.idle_after_secs, 900);
        assert_eq!(p.schedule.len(), 1);
        assert_eq!(p.gpu_max_temp_c, Some(80));
        assert_eq!(p.device_memory_bytes, Some(6 * GIB));
        assert_eq!(p.max_ram_bytes, base.max_ram_bytes);
        assert_eq!(p.yield_to_other_gpu_apps, base.yield_to_other_gpu_apps);
        let back = apply(
            p,
            &SetArgs {
                schedule: vec!["always".into()],
                max_temp: Some(Limit(None)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(back.schedule.is_empty());
        assert_eq!(back.gpu_max_temp_c, None);
        let bad = SetArgs {
            schedule: vec!["always".into(), "mon 08:00-09:00".into()],
            ..Default::default()
        };
        assert!(apply(Policy::default(), &bad).is_err());
        assert!(on_off("maybe").is_err());
        assert_eq!(number_or_off("off"), Ok(Limit(None)));
    }

    #[test]
    fn a_machine_without_workers_reads_offline() {
        let snaps = vec![(
            PathBuf::from("/nowhere"),
            Snapshot {
                error: Some("the worker is not running (or not installed)".into()),
                ..Default::default()
            },
        )];
        let text = render(Path::new("/nowhere"), &snaps, false);
        assert!(text.contains("OFFLINE"), "{text}");
        assert!(text.contains("not installed"), "{text}");
        assert!(!text.contains('\x1b'));
    }
}
