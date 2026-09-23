//! A worker configured with an enrollment code attaches itself to the contributor account on its
//! first start, then works: no operator authorization. The server is simulated; the worker is the
//! real agent (it stops at its first claim, which the simulated server answers with 426).

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use gg_protocol::keys::SecretKey;
use gg_worker::agent::Agent;
use gg_worker::config::WorkerConfig;
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Seen {
    calls: Arc<Mutex<Vec<String>>>,
    enrolled_with: Arc<Mutex<Option<(String, String)>>>,
}

#[tokio::test]
async fn a_worker_with_an_enrollment_code_attaches_itself_and_works() {
    let server_key = SecretKey::generate().unwrap().public_key();
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/v1/info",
            get(move || async move {
                axum::Json(json!({ "network_id": "gg-enroll", "protocol_version": gg_protocol::PROTOCOL_VERSION,
                    "catalog_version": 1, "server_public_key": server_key.to_hex(), "dev_mode": true }))
            }),
        )
        .route(
            "/v1/worker/register",
            post(|State(s): State<Seen>| async move {
                s.calls.lock().unwrap().push("register".into());
                let enrolled = s.enrolled_with.lock().unwrap().is_some();
                axum::Json(json!({ "worker_id": "01900000-0000-7000-8000-000000000002", "authorized": enrolled }))
            }),
        )
        .route(
            "/v1/worker/challenge",
            post(|| async { axum::Json(json!({ "nonce": "00".repeat(32) })) }),
        )
        .route(
            "/v1/worker/session",
            post(|State(s): State<Seen>| async move {
                s.calls.lock().unwrap().push("session".into());
                axum::Json(json!({ "token": "ggs_machine", "expires_in_secs": 900,
                    "principal_id": "01900000-0000-7000-8000-000000000002", "principal_kind": "worker" }))
            }),
        )
        .route(
            "/v1/worker/enroll",
            post(|State(s): State<Seen>, headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                s.calls.lock().unwrap().push("enroll".into());
                let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                *s.enrolled_with.lock().unwrap() =
                    Some((bearer, body["enrollment_token"].as_str().unwrap_or("").to_owned()));
                axum::Json(json!({ "worker_id": "01900000-0000-7000-8000-000000000002", "authorized": true }))
            }),
        )
        .route(
            "/v1/worker/claim",
            post(|State(s): State<Seen>| async move {
                s.calls.lock().unwrap().push("claim".into());
                (
                    StatusCode::UPGRADE_REQUIRED,
                    axum::Json(json!({ "code": "upgrade_required", "message": "stop here" })),
                )
                    .into_response()
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
        enrollment_token: "gge_account-code-for-the-test".into(),
        ..WorkerConfig::default()
    };
    let mut agent = Agent::new(cfg).await.unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), agent.run(rx, None))
        .await
        .expect("the worker went on to claim");
    assert!(outcome.is_err(), "stopped by the simulated 426");
    let (bearer, token) = seen
        .enrolled_with
        .lock()
        .unwrap()
        .clone()
        .expect("the worker enrolled");
    assert_eq!(
        bearer, "Bearer ggs_machine",
        "enrolled with its own session: the key is proven"
    );
    assert_eq!(token, "gge_account-code-for-the-test");
    let calls = seen.calls.lock().unwrap().clone();
    let enroll = calls.iter().position(|c| c == "enroll").unwrap();
    let claim = calls
        .iter()
        .position(|c| c == "claim")
        .expect("claimed after enrolling");
    assert!(enroll < claim, "{calls:?}");
}
