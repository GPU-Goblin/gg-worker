//! The lease keeper heartbeats for the whole attempt, reports when the server says the lease is
//! gone, and stops when dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use gg_client::client::Api;
use gg_core::AttemptId;
use gg_worker::agent::LeaseKeeper;
use serde_json::json;

async fn serve(beats: Arc<AtomicUsize>, gone_after: usize) -> String {
    let app = Router::new()
        .route(
            "/v1/worker/attempts/{id}/heartbeat",
            post(
                |State((beats, gone_after)): State<(Arc<AtomicUsize>, usize)>| async move {
                    let n = beats.fetch_add(1, Ordering::SeqCst) + 1;
                    if n > gone_after {
                        (
                            StatusCode::CONFLICT,
                            axum::Json(json!({ "code": "conflict", "message": "lease expired" })),
                        )
                            .into_response()
                    } else {
                        axum::Json(json!({ "lease_expires_at": 0 })).into_response()
                    }
                },
            ),
        )
        .with_state((beats, gone_after));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_keeper_heartbeats_until_dropped() {
    let beats = Arc::new(AtomicUsize::new(0));
    let base = serve(beats.clone(), usize::MAX).await;
    let keeper = LeaseKeeper::start(
        Api::new(&base).unwrap(),
        "ggs_test".into(),
        AttemptId::new(),
        Duration::from_millis(40),
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let during = beats.load(Ordering::SeqCst);
    assert!(during >= 5, "heartbeats while the attempt lives: {during}");
    assert!(keeper.lost().is_none());
    drop(keeper);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = beats.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        beats.load(Ordering::SeqCst),
        after,
        "no heartbeat once dropped"
    );
}

#[tokio::test]
async fn the_keeper_reports_a_lost_lease_and_stops() {
    let beats = Arc::new(AtomicUsize::new(0));
    let base = serve(beats.clone(), 2).await;
    let keeper = LeaseKeeper::start(
        Api::new(&base).unwrap(),
        "ggs_test".into(),
        AttemptId::new(),
        Duration::from_millis(30),
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let why = keeper.lost().expect("the lost lease is reported");
    assert!(why.contains("lease expired"), "{why}");
    assert_eq!(
        beats.load(Ordering::SeqCst),
        3,
        "stops at the first refusal"
    );
}
