//! The owner's control API (ADR-0043): on this machine only, behind the token only the user can
//! read; pause, immediate pause, resume, policy read and written back to its file.

use std::time::Duration;

use gg_worker::control::{self, Control, Policy, Window};
use serde_json::json;

#[tokio::test]
async fn the_owner_controls_the_worker_through_the_local_api() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let ctl = Control::new(path.clone(), "cpu").unwrap();
    let token = control::token(&dir.path().join("control.token")).unwrap();
    let (_stop, rx) = tokio::sync::watch::channel(false);
    // Not on the network.
    assert!(
        control::serve(ctl.clone(), "0.0.0.0:0", token.clone(), rx.clone())
            .await
            .is_err()
    );
    let addr = control::serve(ctl.clone(), "127.0.0.1:0", token.clone(), rx)
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let url = |p: &str| format!("http://{addr}/v1{p}");

    // No token, a wrong token: refused.
    let r = http.get(url("/status")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = http
        .get(url("/status"))
        .bearer_auth("0".repeat(64))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let s: serde_json::Value = http
        .get(url("/status"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["state"], "starting");
    assert_eq!(s["policy"]["paused"], false);

    // An immediate pause: written to the file, and the running task is told to stop.
    let mut aborts = ctl.aborts();
    let s: serde_json::Value = http
        .post(url("/pause"))
        .bearer_auth(&token)
        .json(&json!({ "now": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["policy"]["paused"], true);
    tokio::time::timeout(Duration::from_secs(1), aborts.changed())
        .await
        .expect("the running task is aborted")
        .unwrap();
    assert!(Policy::load(&path).unwrap().paused);

    // A policy that cannot be meant is refused; a sensible one is kept.
    let mut p = Policy {
        schedule: vec![Window {
            days: vec!["sat".into()],
            from: "25:00".into(),
            to: "07:00".into(),
        }],
        ..Policy::default()
    };
    let r = http
        .put(url("/policy"))
        .bearer_auth(&token)
        .json(&p)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    p.schedule[0].from = "22:00".into();
    p.gpu_max_temp_c = Some(78);
    p.only_when_idle = true;
    let r = http
        .put(url("/policy"))
        .bearer_auth(&token)
        .json(&p)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(Policy::load(&path).unwrap(), p);

    // Resume.
    let s: serde_json::Value = http
        .post(url("/resume"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["policy"]["paused"], false);
    assert_eq!(s["policy"]["gpu_max_temp_c"], 78);
}
