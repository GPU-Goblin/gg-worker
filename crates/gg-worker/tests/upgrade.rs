//! A worker older than the server accepts is told so (426) when it asks for a task: it stops,
//! saying it must be updated, instead of retrying forever. The server is simulated; the worker is
//! the real agent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use gg_protocol::keys::SecretKey;
use gg_worker::agent::Agent;
use gg_worker::config::WorkerConfig;
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Seen {
    claims: Arc<AtomicUsize>,
    version: Arc<std::sync::Mutex<Option<String>>>,
}

async fn claim(State(seen): State<Seen>, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
    seen.claims.fetch_add(1, Ordering::SeqCst);
    *seen.version.lock().unwrap() = body["agent_version"].as_str().map(str::to_owned);
    (
        StatusCode::UPGRADE_REQUIRED,
        axum::Json(json!({
            "code": "upgrade_required",
            "message": "gg-worker 0.1.0 is older than 9.0.0, the oldest version this server gives tasks to",
        })),
    )
}

#[tokio::test]
async fn a_worker_told_to_update_stops_instead_of_retrying() {
    let server_key = SecretKey::generate().unwrap().public_key();
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/v1/info",
            get(move || async move {
                axum::Json(json!({
                    "network_id": "gg-upgrade", "protocol_version": gg_protocol::PROTOCOL_VERSION,
                    "catalog_version": 1, "server_public_key": server_key.to_hex(), "dev_mode": true,
                }))
            }),
        )
        .route(
            "/v1/worker/register",
            post(|| async {
                axum::Json(json!({ "worker_id": "01900000-0000-7000-8000-000000000001", "authorized": true }))
            }),
        )
        .route(
            "/v1/worker/challenge",
            post(|| async { axum::Json(json!({ "nonce": "00".repeat(32) })) }),
        )
        .route(
            "/v1/worker/session",
            post(|| async {
                axum::Json(json!({ "token": "ggs_test", "expires_in_secs": 900,
                                   "principal_id": "01900000-0000-7000-8000-000000000001", "principal_kind": "worker" }))
            }),
        )
        .route("/v1/worker/claim", post(claim))
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
        ..WorkerConfig::default()
    };
    let mut agent = Agent::new(cfg).await.unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), agent.run(rx, None))
        .await
        .expect("the worker stops on its own");
    let e = outcome
        .expect_err("an outdated worker must stop with an error")
        .to_string();
    assert!(e.contains("update required") && e.contains("9.0.0"), "{e}");
    assert_eq!(
        seen.claims.load(Ordering::SeqCst),
        1,
        "no retry after a 426"
    );
    assert_eq!(
        seen.version.lock().unwrap().as_deref(),
        Some(env!("CARGO_PKG_VERSION")),
        "the worker sends its version with each claim"
    );
}
