//! The owner's policy holds the real agent back (ADR-0043): paused, it asks the server for nothing;
//! resumed through the control API, it asks again; paused again, it stops asking. The server is
//! simulated and never has work.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use gg_protocol::keys::SecretKey;
use gg_worker::agent::Agent;
use gg_worker::config::WorkerConfig;
use gg_worker::control::{Control, Policy};
use serde_json::json;

#[derive(Clone, Default)]
struct Seen {
    claims: Arc<Mutex<u32>>,
}

async fn claims_after(seen: &Seen, wait: Duration) -> u32 {
    tokio::time::sleep(wait).await;
    *seen.claims.lock().unwrap()
}

#[tokio::test]
async fn a_paused_worker_asks_for_nothing_until_resumed() {
    let server_key = SecretKey::generate().unwrap().public_key();
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/v1/info",
            get(move || async move {
                axum::Json(json!({ "network_id": "gg-policy", "protocol_version": gg_protocol::PROTOCOL_VERSION,
                    "catalog_version": 1, "server_public_key": server_key.to_hex(), "dev_mode": true }))
            }),
        )
        .route(
            "/v1/worker/register",
            post(|| async {
                axum::Json(json!({ "worker_id": "01900000-0000-7000-8000-000000000003", "authorized": true }))
            }),
        )
        .route(
            "/v1/worker/challenge",
            post(|| async { axum::Json(json!({ "nonce": "00".repeat(32) })) }),
        )
        .route(
            "/v1/worker/session",
            post(|| async {
                axum::Json(json!({ "token": "ggs_machine", "expires_in_secs": 900,
                    "principal_id": "01900000-0000-7000-8000-000000000003", "principal_kind": "worker" }))
            }),
        )
        .route(
            "/v1/worker/me",
            get(|| async {
                axum::Json(json!({ "worker_id": "01900000-0000-7000-8000-000000000003",
                    "earned": "12", "accepted": 7 }))
            }),
        )
        .route(
            "/v1/worker/claim",
            post(|State(s): State<Seen>| async move {
                *s.claims.lock().unwrap() += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
                StatusCode::NO_CONTENT
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let cfg = WorkerConfig {
        server_url: format!("http://{addr}"),
        key_path: dir.path().join("w.key"),
        work_dir: dir.path().join("work"),
        sandbox: "none".into(),
        dev_unsafe_no_isolation: true,
        cgroups: "off".into(),
        claim_wait_secs: 0,
        ..WorkerConfig::default()
    };
    // Paused from the start.
    let path = dir.path().join("policy.toml");
    Policy {
        paused: true,
        ..Policy::default()
    }
    .save(&path)
    .unwrap();
    let ctl = Control::new(path, "cpu").unwrap();
    let mut agent = Agent::new(cfg).await.unwrap();
    agent.with_control(ctl.clone());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(async move { agent.run(rx, None).await });

    assert_eq!(claims_after(&seen, Duration::from_millis(800)).await, 0);
    let s = ctl.status();
    assert_eq!(s.state, "waiting");
    assert_eq!(
        serde_json::to_value(&s.waiting).unwrap(),
        json!({ "reason": "paused" })
    );
    // What the machine earned, for the dashboard.
    assert_eq!(s.earned.as_deref(), Some("12"));
    assert_eq!(s.accepted, Some(7));

    // Resumed: it asks at once, and keeps asking.
    ctl.resume().unwrap();
    let asked = claims_after(&seen, Duration::from_millis(800)).await;
    assert!(asked >= 2, "{asked}");

    // Paused again: it stops asking.
    ctl.pause(false).unwrap();
    let before = claims_after(&seen, Duration::from_millis(200)).await;
    let after = claims_after(&seen, Duration::from_millis(800)).await;
    assert!(after <= before + 1, "{before} then {after}");
    assert_eq!(ctl.status().state, "waiting");

    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), running).await;
}
