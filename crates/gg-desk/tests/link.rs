//! The application's link to a worker's real control API (ADR-0043): found through the files the
//! worker writes; its status read; pause, resume and a new policy carried out and written by the
//! worker.

use std::sync::Arc;
use std::time::Duration;

use gg_desk::link::{Link, Order};
use gg_worker::control::{self, Control, Policy, Window};

async fn until(link: &Link, what: &str, f: impl Fn(&gg_desk::link::Snapshot) -> bool) {
    let started = std::time::Instant::now();
    while !f(&link.snapshot()) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}: {:?}",
            link.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_application_drives_the_worker_through_its_files_and_api() {
    let dir = tempfile::tempdir().unwrap();
    let link = Link::start(dir.path().to_path_buf(), Arc::new(|| {}));
    // No worker yet: the application says so.
    until(&link, "the missing worker", |s| {
        s.error
            .as_deref()
            .is_some_and(|e| e.contains("not running"))
    })
    .await;
    assert!(!link.snapshot().connected());

    // The worker starts: it writes its address and token next to its policy.
    let policy = dir.path().join("policy.toml");
    let ctl = Control::new(policy.clone(), "gpu").unwrap();
    let token = control::token(&dir.path().join("control.token")).unwrap();
    let (_stop, rx) = tokio::sync::watch::channel(false);
    let addr = control::serve(ctl.clone(), "127.0.0.1:0", token, rx)
        .await
        .unwrap();
    std::fs::write(dir.path().join("control.addr"), addr.to_string()).unwrap();
    until(&link, "the worker", |s| s.connected()).await;
    assert_eq!(link.snapshot().status.unwrap().engine, "gpu");

    link.send(Order::Pause { now: false });
    until(&link, "the pause", |s| {
        s.status.as_ref().is_some_and(|st| st.policy.paused)
    })
    .await;
    assert!(Policy::load(&policy).unwrap().paused);

    let wanted = Policy {
        only_when_idle: true,
        gpu_max_temp_c: Some(80),
        schedule: vec![Window {
            days: vec!["sat".into(), "sun".into()],
            from: "09:00".into(),
            to: "21:00".into(),
        }],
        ..Policy::default()
    };
    link.send(Order::SetPolicy(wanted.clone()));
    until(&link, "the new policy", |s| {
        s.status.as_ref().is_some_and(|st| st.policy == wanted)
    })
    .await;
    assert_eq!(Policy::load(&policy).unwrap(), wanted);

    // A policy the worker refuses: its reason is shown, nothing changes.
    let mut wrong = wanted.clone();
    wrong.gpu_max_temp_c = Some(200);
    link.send(Order::SetPolicy(wrong));
    until(&link, "the refusal", |s| {
        s.error
            .as_deref()
            .is_some_and(|e| e.contains("gpu_max_temp_c"))
    })
    .await;
    assert_eq!(Policy::load(&policy).unwrap(), wanted);

    link.send(Order::Resume);
    until(&link, "the resumption", |s| {
        s.status.as_ref().is_some_and(|st| !st.policy.paused)
    })
    .await;
}

/// A worker's control API in `dir`, as `gg-worker run` starts it.
async fn worker(
    dir: &std::path::Path,
    engine: &str,
) -> (Arc<Control>, tokio::sync::watch::Sender<bool>) {
    std::fs::create_dir_all(dir).unwrap();
    let ctl = Control::new(dir.join("policy.toml"), engine).unwrap();
    let token = control::token(&dir.join("control.token")).unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let addr = control::serve(ctl.clone(), "127.0.0.1:0", token, rx)
        .await
        .unwrap();
    std::fs::write(dir.join("control.addr"), addr.to_string()).unwrap();
    std::fs::write(dir.join("worker.key"), b"").unwrap();
    (ctl, stop)
}

fn desk(base: &std::path::Path, args: &[&str]) -> std::process::Output {
    // `GG_TEST_DESK`: tests built on another machine carry that machine's path.
    std::process::Command::new(
        std::env::var_os("GG_TEST_DESK").unwrap_or_else(|| env!("CARGO_BIN_EXE_gg-desk").into()),
    )
    .arg("--dir")
    .arg(base)
    .args(args)
    .env_remove("DISPLAY")
    .env_remove("WAYLAND_DISPLAY")
    .output()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machine_with_two_cards_is_one_machine_with_two_workers() {
    let base = tempfile::tempdir().unwrap();
    let (_a, _sa) = worker(base.path(), "gpu:0").await;
    let (_b, _sb) = worker(&base.path().join("gpu1"), "gpu:1").await;
    // Not a worker: ignored.
    std::fs::create_dir_all(base.path().join("gpu7")).unwrap();

    let link = Link::start(base.path().to_path_buf(), Arc::new(|| {}));
    until(&link, "both workers", |_| {
        let m = link.members();
        m.len() == 2 && m.iter().all(|m| m.snap.connected())
    })
    .await;
    let labels: Vec<String> = link.members().into_iter().map(|m| m.label).collect();
    assert_eq!(labels, ["GPU 0", "GPU 1"]);

    // One card paused: the machine still lends.
    link.send_to(1, Order::Pause { now: false });
    until(&link, "the second card's pause", |_| {
        link.members()[1]
            .snap
            .status
            .as_ref()
            .is_some_and(|s| s.policy.paused)
    })
    .await;
    assert!(
        !link.members()[0]
            .snap
            .status
            .as_ref()
            .unwrap()
            .policy
            .paused
    );
    assert!(!link.snapshot().status.unwrap().policy.paused);
    link.send(Order::Pause { now: false });
    until(&link, "the machine's pause", |s| {
        s.status.as_ref().is_some_and(|st| st.policy.paused)
    })
    .await;

    // The console, as on a server: no display, the same workers.
    let out = tokio::task::spawn_blocking({
        let b = base.path().to_path_buf();
        move || desk(&b, &["resume", "--gpu", "0"])
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("GPU 0") && text.contains("GPU 1"), "{text}");
    let b = base.path().to_path_buf();
    let json = tokio::task::spawn_blocking(move || desk(&b, &["status", "--json"]))
        .await
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v[0]["worker"], "GPU 0");
    assert_eq!(v[0]["status"]["policy"]["paused"], false);
    assert_eq!(v[1]["status"]["policy"]["paused"], true);

    let b = base.path().to_path_buf();
    let set = tokio::task::spawn_blocking(move || {
        desk(
            &b,
            &[
                "set",
                "--schedule",
                "mon-fri 18:00-08:00",
                "--max-temp",
                "78",
            ],
        )
    })
    .await
    .unwrap();
    assert!(set.status.success(), "{set:?}");
    for d in [base.path().to_path_buf(), base.path().join("gpu1")] {
        let p = Policy::load(&d.join("policy.toml")).unwrap();
        assert_eq!(p.gpu_max_temp_c, Some(78));
        assert_eq!(p.schedule[0].days.len(), 5);
    }
    // A card that is not there, a value that cannot be meant: refused, nothing changed.
    let b = base.path().to_path_buf();
    let wrong = tokio::task::spawn_blocking(move || desk(&b, &["pause", "--gpu", "5"]))
        .await
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("no worker on GPU 5"));
    let b = base.path().to_path_buf();
    let wrong = tokio::task::spawn_blocking(move || desk(&b, &["set", "--max-temp", "300"]))
        .await
        .unwrap();
    assert!(!wrong.status.success());
    assert_eq!(
        Policy::load(&base.path().join("policy.toml"))
            .unwrap()
            .gpu_max_temp_c,
        Some(78)
    );

    // No worker at all: the status says so, with its own exit code.
    let empty = tempfile::tempdir().unwrap();
    let e = empty.path().to_path_buf();
    let none = tokio::task::spawn_blocking(move || desk(&e, &["status"]))
        .await
        .unwrap();
    assert_eq!(none.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&none.stdout).contains("OFFLINE"));
}
