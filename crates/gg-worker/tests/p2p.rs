//! Peer-assisted distribution (ADR-0012), pass criteria of P5: bytes obtained from peers are
//! verified chunk by chunk against a server-signed manifest, a peer that lies is dropped and never
//! contributes a byte, a peer that disappears mid-transfer costs nothing, and the control plane
//! remains the durable source. The sharing side refuses everything but a valid server ticket.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use gg_client::cache::ArtifactCache;
use gg_client::client::Api;
use gg_core::{NetworkId, Sha256Digest, UnixMillis, WorkerId};
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::{ArtifactRef, ChunkManifestV1, ChunkRequestV1, PeerTicketV1};
use gg_protocol::signed::Signed;
use gg_worker::p2p::{
    FetchConfig, PeerServer, REQUEST_HEADER, TICKET_HEADER, fetch_with_peers, peer_addr_allowed,
};
use gg_worker::peer_tls::{TlsIdentity, peer_get, serve_tls};

const CHUNK: u32 = 64 * 1024;

fn net() -> NetworkId {
    NetworkId::new("gg-p2p-test").unwrap()
}

fn blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 13) % 251) as u8).collect()
}

fn manifest_of(data: &[u8], hash: Sha256Digest) -> ChunkManifestV1 {
    ChunkManifestV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: net(),
        artifact_hash: hash,
        size: data.len() as u64,
        chunk_size: CHUNK,
        chunks: data
            .chunks(CHUNK as usize)
            .map(Sha256Digest::of_bytes)
            .collect(),
        issued_at: UnixMillis::now(),
    }
}

/// Fake control plane: signs manifests and tickets, serves ranges, counts what it served.
#[derive(Clone)]
struct ControlPlane {
    data: Arc<Vec<u8>>,
    hash: Sha256Digest,
    key: Arc<SecretKey>,
    peers: Arc<Vec<(String, Sha256Digest)>>,
    ranges_served: Arc<AtomicUsize>,
    reader_key: Arc<gg_protocol::keys::PublicKey>,
}

async fn cp_manifest(State(cp): State<ControlPlane>) -> Response {
    let m = manifest_of(&cp.data, cp.hash);
    let signed = Signed::sign(&cp.key, &net(), Domain::ChunkManifest, m).unwrap();
    axum::Json(serde_json::json!({ "manifest": signed })).into_response()
}

async fn cp_peers(State(cp): State<ControlPlane>) -> Response {
    let now = UnixMillis::now();
    let ticket = PeerTicketV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: net(),
        artifact_hash: cp.hash,
        size: cp.data.len() as u64,
        chunk_size: CHUNK,
        reader_id: WorkerId::from_u128(42).into(),
        reader_key: *cp.reader_key,
        issued_at: now,
        expires_at: UnixMillis(now.0 + 300_000),
    };
    let signed = Signed::sign(&cp.key, &net(), Domain::PeerTicket, ticket).unwrap();
    let peers: Vec<serde_json::Value> = cp
        .peers
        .iter()
        .enumerate()
        .map(|(i, (addr, fp))| {
            serde_json::json!({
                "worker_id": WorkerId::from_u128(i as u128 + 1),
                "addr": addr,
                "tls_fingerprint": fp,
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "peers": peers, "ticket": signed })).into_response()
}

async fn cp_range(State(cp): State<ControlPlane>, headers: HeaderMap) -> Response {
    cp.ranges_served.fetch_add(1, Ordering::SeqCst);
    let size = cp.data.len() as u64;
    let (start, end) = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .map(|(a, b)| {
            (
                a.parse::<u64>().unwrap_or(0),
                b.parse::<u64>().unwrap_or(size - 1),
            )
        })
        .unwrap_or((0, size - 1));
    let slice = cp.data[start as usize..=(end as usize).min(cp.data.len() - 1)].to_vec();
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("content-length", slice.len())
        .header("content-range", format!("bytes {start}-{end}/{size}"))
        .body(Body::from(slice))
        .unwrap()
}

async fn start_control_plane(cp: ControlPlane) -> String {
    let hash = cp.hash.to_hex();
    let app = Router::new()
        .route(
            &format!("/v1/worker/artifacts/{hash}/manifest"),
            get(cp_manifest),
        )
        .route(&format!("/v1/worker/artifacts/{hash}/peers"), get(cp_peers))
        .route(&format!("/v1/worker/artifacts/{hash}"), get(cp_range))
        .with_state(cp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A peer that answers every chunk request with subtly wrong bytes.
#[derive(Clone)]
struct LyingPeer {
    data: Arc<Vec<u8>>,
    served: Arc<AtomicUsize>,
}

async fn lying_chunk(
    State(p): State<LyingPeer>,
    Path((_hash, index)): Path<(String, u32)>,
) -> Response {
    p.served.fetch_add(1, Ordering::SeqCst);
    let start = index as usize * CHUNK as usize;
    if start >= p.data.len() {
        return (StatusCode::RANGE_NOT_SATISFIABLE, "out of range").into_response();
    }
    let end = (start + CHUNK as usize).min(p.data.len());
    let mut bytes = p.data[start..end].to_vec();
    bytes[0] ^= 0xff; // one flipped bit is enough to be caught
    (StatusCode::OK, bytes).into_response()
}

/// The lying peer speaks the same TLS transport as an honest one: what is caught is the content,
/// not the connection.
async fn start_lying_peer(
    peer: LyingPeer,
) -> (String, Sha256Digest, tokio::sync::watch::Sender<bool>) {
    let app = Router::new()
        .route("/p2p/v1/artifacts/{hash}/chunks/{index}", get(lying_chunk))
        .with_state(peer);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let identity = TlsIdentity::generate().unwrap();
    let fingerprint = identity.fingerprint();
    let config = identity.server_config().unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(serve_tls(listener, app, config, 8, rx));
    // The caller keeps the sender: dropping it stops the peer.
    (addr.to_string(), fingerprint, tx)
}

struct Fixture {
    _dir: tempfile::TempDir,
    cache: ArtifactCache,
    data: Vec<u8>,
    hash: Sha256Digest,
    server_key: Arc<SecretKey>,
    reader: SecretKey,
}

fn fixture(len: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let cache = ArtifactCache::new(dir.path().join("cache"), 1 << 30);
    let data = blob(len);
    let hash = Sha256Digest::of_bytes(&data);
    let src = dir.path().join("source");
    std::fs::write(&src, &data).unwrap();
    cache.store(&hash, &src).unwrap();
    Fixture {
        _dir: dir,
        cache,
        data,
        hash,
        server_key: Arc::new(SecretKey::generate().unwrap()),
        reader: SecretKey::generate().unwrap(),
    }
}

fn cfg() -> FetchConfig {
    FetchConfig {
        max_peers: 8,
        allow_private_peers: true,
        request_timeout_secs: 5,
        parallel_chunks: 4,
        partials: None,
        self_fingerprint: None,
        swarm_patience_ms: 2000,
        tracker: "/worker".into(),
    }
}

#[tokio::test]
async fn honest_peer_serves_every_chunk_and_the_control_plane_is_not_touched() {
    let f = fixture(200_000); // 4 chunks
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let cp = ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        peers: Arc::new(vec![(holder.addr().to_string(), holder.fingerprint())]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(f.reader.public_key()),
    };
    let base = start_control_plane(cp.clone()).await;
    let api = Api::new(&base).unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("artifact");
    let stats = fetch_with_peers(
        &api,
        "token",
        &f.reader,
        &net(),
        &f.server_key.public_key(),
        &ArtifactRef {
            hash: f.hash,
            size: f.data.len() as u64,
        },
        &format!("/worker/artifacts/{}", f.hash),
        &dest,
        &cfg(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), f.data);
    assert_eq!(stats.chunks_from_peers, 4);
    assert_eq!(stats.chunks_from_http, 0);
    assert_eq!(stats.bad_chunks, 0);
    // At most one connection per download slot, never one per chunk.
    assert!(stats.peer_connections as usize <= cfg().parallel_chunks);
    assert_eq!(cp.ranges_served.load(Ordering::SeqCst), 0);
    let (chunks, bytes, unauthorized, missing, budget) = holder.stats().snapshot();
    assert_eq!(chunks, 4);
    assert_eq!(bytes, f.data.len() as u64);
    assert_eq!((unauthorized, missing, budget), (0, 0, 0));
    holder.stop().await;
}

/// A lying peer facing `parallel` requests in flight. With one chunk at a time, it is caught at its
/// first lie and never asked again. In parallel, requests already sent to it before the detection
/// may still reach it: the exposure is bounded by the number in flight, and nothing it sends is
/// written.
async fn lying_peer_case(parallel: usize) {
    let f = fixture(200_000); // 4 chunks
    let liar = LyingPeer {
        data: Arc::new(f.data.clone()),
        served: Arc::new(AtomicUsize::new(0)),
    };
    let (liar_addr, liar_fingerprint, _liar_alive) = start_lying_peer(liar.clone()).await;
    let honest = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let cp = ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        // The liar is tried first for chunk 0; peers rotate, so both are exercised.
        peers: Arc::new(vec![
            (liar_addr.clone(), liar_fingerprint),
            (honest.addr().to_string(), honest.fingerprint()),
        ]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(f.reader.public_key()),
    };
    let base = start_control_plane(cp.clone()).await;
    let api = Api::new(&base).unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("artifact");
    let stats = fetch_with_peers(
        &api,
        "token",
        &f.reader,
        &net(),
        &f.server_key.public_key(),
        &ArtifactRef {
            hash: f.hash,
            size: f.data.len() as u64,
        },
        &format!("/worker/artifacts/{}", f.hash),
        &dest,
        &FetchConfig {
            parallel_chunks: parallel,
            ..cfg()
        },
    )
    .await
    .unwrap();
    // The file is exactly the original: the corrupted chunk never reached it.
    assert_eq!(std::fs::read(&dest).unwrap(), f.data);
    assert!(
        stats.bad_chunks >= 1 && stats.bad_chunks as usize <= parallel,
        "{}",
        stats.bad_chunks
    );
    assert_eq!(stats.peers_dropped, vec![liar_addr.clone()]);
    assert_eq!(stats.chunks_from_peers + stats.chunks_from_http, 4);
    // After being caught the liar is never asked again: it served at most the chunks it was tried
    // for before detection — one when chunks go one at a time.
    let served = liar.served.load(Ordering::SeqCst);
    if parallel == 1 {
        assert_eq!(served, 1, "a dropped peer must never be queried again");
    } else {
        assert!(served <= parallel, "{served}");
    }
    assert_eq!(
        stats.chunks_from_peers, 4,
        "the honest peer served every chunk"
    );
    honest.stop().await;
}

#[tokio::test]
async fn a_peer_serving_false_bytes_is_dropped_and_never_contributes() {
    lying_peer_case(1).await;
}

#[tokio::test]
async fn a_lying_peer_is_dropped_with_several_chunks_in_flight() {
    lying_peer_case(4).await;
}

#[tokio::test]
async fn a_peer_that_disappears_mid_transfer_falls_back_to_the_control_plane() {
    let f = fixture(200_000); // 4 chunks
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let addr = holder.addr().to_string();
    let fingerprint = holder.fingerprint();
    let cp = ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        peers: Arc::new(vec![(addr, fingerprint)]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(f.reader.public_key()),
    };
    let base = start_control_plane(cp.clone()).await;
    let api = Api::new(&base).unwrap();
    // The only peer goes away before the transfer starts: nothing blocks, every chunk comes from
    // the durable source.
    holder.stop().await;
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("artifact");
    let stats = fetch_with_peers(
        &api,
        "token",
        &f.reader,
        &net(),
        &f.server_key.public_key(),
        &ArtifactRef {
            hash: f.hash,
            size: f.data.len() as u64,
        },
        &format!("/worker/artifacts/{}", f.hash),
        &dest,
        &cfg(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), f.data);
    assert_eq!(stats.chunks_from_peers, 0);
    assert_eq!(stats.chunks_from_http, 4);
    assert_eq!(cp.ranges_served.load(Ordering::SeqCst), 4);
}

/// Build the two headers a holder expects.
fn headers_for(
    ticket: &Signed<PeerTicketV1>,
    reader: &SecretKey,
    hash: Sha256Digest,
    index: u32,
    issued_at: UnixMillis,
) -> Vec<(&'static str, String)> {
    let req = ChunkRequestV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: net(),
        artifact_hash: hash,
        chunk_index: index,
        issued_at,
    };
    let signed = Signed::sign(reader, &net(), Domain::ChunkRequest, req).unwrap();
    vec![
        (
            TICKET_HEADER,
            hex::encode(ticket.to_canonical_bytes().unwrap()),
        ),
        (
            REQUEST_HEADER,
            hex::encode(signed.to_canonical_bytes().unwrap()),
        ),
    ]
}

fn ticket_for(
    key: &SecretKey,
    hash: Sha256Digest,
    size: u64,
    reader: &SecretKey,
    ttl_ms: i64,
) -> Signed<PeerTicketV1> {
    let now = UnixMillis::now();
    Signed::sign(
        key,
        &net(),
        Domain::PeerTicket,
        PeerTicketV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: net(),
            artifact_hash: hash,
            size,
            chunk_size: CHUNK,
            reader_id: WorkerId::from_u128(42).into(),
            reader_key: reader.public_key(),
            issued_at: now,
            expires_at: UnixMillis(now.0 + ttl_ms),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn the_sharing_side_refuses_everything_but_a_valid_server_ticket() {
    let f = fixture(200_000);
    let size = f.data.len() as u64;
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let addr = holder.addr().to_string();
    let fp = holder.fingerprint();
    let timeout = std::time::Duration::from_secs(5);
    // Every call goes through the same pinned-TLS path production uses.
    let ask = async |hash: Sha256Digest, index: u32, headers: Vec<(&'static str, String)>| {
        peer_get(
            &addr,
            fp,
            &format!("/p2p/v1/artifacts/{hash}/chunks/{index}"),
            &headers,
            CHUNK as usize,
            timeout,
        )
        .await
        .unwrap()
    };
    let good = ticket_for(&f.server_key, f.hash, size, &f.reader, 300_000);

    // No headers at all.
    assert_eq!(ask(f.hash, 0, vec![]).await.0, 403);

    // Ticket signed by someone else than the server.
    let impostor = SecretKey::generate().unwrap();
    let forged = ticket_for(&impostor, f.hash, size, &f.reader, 300_000);
    let h = headers_for(&forged, &f.reader, f.hash, 0, UnixMillis::now());
    assert_eq!(ask(f.hash, 0, h).await.0, 403);

    // Valid ticket, but the request is signed by another key.
    let other_reader = SecretKey::generate().unwrap();
    let h = headers_for(&good, &other_reader, f.hash, 0, UnixMillis::now());
    assert_eq!(ask(f.hash, 0, h).await.0, 403);

    // Valid ticket for another artifact.
    let other_hash = Sha256Digest::of_bytes(b"another artifact");
    let other_ticket = ticket_for(&f.server_key, other_hash, size, &f.reader, 300_000);
    let h = headers_for(&other_ticket, &f.reader, f.hash, 0, UnixMillis::now());
    assert_eq!(ask(f.hash, 0, h).await.0, 403);

    // Expired ticket.
    let expired = ticket_for(&f.server_key, f.hash, size, &f.reader, -1000);
    let h = headers_for(&expired, &f.reader, f.hash, 0, UnixMillis::now());
    assert_eq!(ask(f.hash, 0, h).await.0, 403);

    // Stale request (replayed long after it was signed).
    let stale = UnixMillis(UnixMillis::now().0 - 10 * 60 * 1000);
    let h = headers_for(&good, &f.reader, f.hash, 0, stale);
    assert_eq!(ask(f.hash, 0, h).await.0, 403);

    // Request for another chunk than the one in the path.
    let h = headers_for(&good, &f.reader, f.hash, 0, UnixMillis::now());
    assert_eq!(ask(f.hash, 1, h).await.0, 403);

    // Chunk past the end.
    let h = headers_for(&good, &f.reader, f.hash, 99, UnixMillis::now());
    assert_eq!(ask(f.hash, 99, h).await.0, 416);

    // A valid ticket for an artifact this holder does not have: nothing is served, and no file
    // outside the cache can be reached (the path is the hash, never a caller-chosen name).
    let absent = Sha256Digest::of_bytes(b"not in this cache");
    let absent_ticket = ticket_for(&f.server_key, absent, size, &f.reader, 300_000);
    let h = headers_for(&absent_ticket, &f.reader, absent, 0, UnixMillis::now());
    assert_eq!(ask(absent, 0, h).await.0, 404);

    // The legitimate request still works.
    let h = headers_for(&good, &f.reader, f.hash, 0, UnixMillis::now());
    let (status, body) = ask(f.hash, 0, h).await;
    assert_eq!(status, 200);
    assert_eq!(body, f.data[..CHUNK as usize]);

    let (chunks, _, unauthorized, missing, _) = holder.stats().snapshot();
    assert_eq!(chunks, 1);
    assert_eq!(unauthorized, 7, "every refusal is counted");
    assert_eq!(missing, 1);
    holder.stop().await;
}

#[tokio::test]
async fn a_peer_reached_with_the_wrong_pinned_key_is_refused_before_any_byte() {
    let f = fixture(200_000);
    let size = f.data.len() as u64;
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let addr = holder.addr().to_string();
    let ticket = ticket_for(&f.server_key, f.hash, size, &f.reader, 300_000);
    let headers = headers_for(&ticket, &f.reader, f.hash, 0, UnixMillis::now());
    // Someone else's transport key: the handshake fails, so no request is ever sent.
    let wrong = TlsIdentity::generate().unwrap().fingerprint();
    let err = peer_get(
        &addr,
        wrong,
        &format!("/p2p/v1/artifacts/{}/chunks/0", f.hash),
        &headers,
        CHUNK as usize,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert!(err.contains("TLS handshake"), "{err}");
    let (chunks, bytes, unauthorized, _, _) = holder.stats().snapshot();
    assert_eq!(
        (chunks, bytes, unauthorized),
        (0, 0, 0),
        "a pinning failure never reaches the application"
    );
    // The same call with the right fingerprint works: the refusal came from the key, not the peer.
    let (status, body) = peer_get(
        &addr,
        holder.fingerprint(),
        &format!("/p2p/v1/artifacts/{}/chunks/0", f.hash),
        &headers,
        CHUNK as usize,
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, f.data[..CHUNK as usize]);
    holder.stop().await;
}

#[tokio::test]
async fn the_upload_budget_bounds_what_a_holder_serves() {
    let f = fixture(200_000);
    let size = f.data.len() as u64;
    // Budget for one chunk only.
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        CHUNK as u64,
    )
    .await
    .unwrap();
    let addr = holder.addr().to_string();
    let ticket = ticket_for(&f.server_key, f.hash, size, &f.reader, 300_000);
    let mut statuses = Vec::new();
    for i in 0..2u32 {
        let headers = headers_for(&ticket, &f.reader, f.hash, i, UnixMillis::now());
        let (status, _) = peer_get(
            &addr,
            holder.fingerprint(),
            &format!("/p2p/v1/artifacts/{}/chunks/{i}", f.hash),
            &headers,
            CHUNK as usize,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        statuses.push(status);
    }
    assert_eq!(statuses, vec![200, 429]);
    let (chunks, bytes, _, _, budget) = holder.stats().snapshot();
    assert_eq!((chunks, bytes, budget), (1, CHUNK as u64, 1));
    holder.stop().await;
}

#[test]
fn pinned_artifacts_survive_eviction_and_peer_addresses_follow_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    // Budget for two 1000-byte entries.
    let cache = ArtifactCache::new(dir.path().join("cache"), 2200);
    let mut hashes = Vec::new();
    for i in 0..3u8 {
        let bytes = vec![i; 1000];
        let hash = Sha256Digest::of_bytes(&bytes);
        let src = dir.path().join(format!("src{i}"));
        std::fs::write(&src, &bytes).unwrap();
        if i == 0 {
            cache.pin(&hash); // used by an attempt in progress
        }
        cache.store(&hash, &src).unwrap();
        hashes.push(hash);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(cache.is_pinned(&hashes[0]));
    assert!(
        cache.path_of(&hashes[0]).exists(),
        "a pinned artifact is never evicted"
    );
    assert!(
        !cache.path_of(&hashes[1]).exists(),
        "oldest unpinned evicted"
    );
    cache.unpin(&hashes[0]);
    assert!(!cache.is_pinned(&hashes[0]));

    let allowed = |s: &str, private: bool| peer_addr_allowed(&s.parse().unwrap(), private);
    assert!(allowed("203.0.113.9:7000", false));
    assert!(!allowed("127.0.0.1:7000", false));
    assert!(allowed("127.0.0.1:7000", true));
    assert!(!allowed("169.254.0.1:7000", true));
    assert!(!allowed("0.0.0.0:7000", true));
}

#[tokio::test]
async fn a_peer_reached_over_ipv6_serves_chunks_the_same_way() {
    let f = fixture(200_000);
    // IPv6 needs no NAT traversal: a worker with a global address is directly reachable. Here the
    // loopback stands in for it; everything above the socket is identical.
    let holder = match PeerServer::start(
        "[::1]:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("skipping: no IPv6 loopback on this machine ({e})");
            return;
        }
    };
    let addr = holder.addr();
    assert!(addr.is_ipv6(), "the holder listens on IPv6");
    // The announced form keeps the brackets, which is what the control plane stores and what the
    // reader connects to.
    assert!(
        addr.to_string().starts_with("[::1]:"),
        "unexpected address form {addr}"
    );
    let cp = ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        peers: Arc::new(vec![(addr.to_string(), holder.fingerprint())]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(f.reader.public_key()),
    };
    let base = start_control_plane(cp.clone()).await;
    let api = Api::new(&base).unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("artifact");
    let stats = fetch_with_peers(
        &api,
        "token",
        &f.reader,
        &net(),
        &f.server_key.public_key(),
        &ArtifactRef {
            hash: f.hash,
            size: f.data.len() as u64,
        },
        &format!("/worker/artifacts/{}", f.hash),
        &dest,
        &cfg(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), f.data);
    assert_eq!(stats.chunks_from_peers, 4, "every chunk came over IPv6");
    assert_eq!(cp.ranges_served.load(Ordering::SeqCst), 0);
    holder.stop().await;
}

#[test]
fn only_globally_routable_ipv6_addresses_are_announced() {
    use gg_worker::nat_probe::is_global_ipv6;
    let global: std::net::Ipv6Addr = "2a01:cb19:1::1".parse().unwrap();
    assert!(is_global_ipv6(global));
    for local in [
        "::1",                 // loopback
        "fe80::1",             // link-local
        "fd00::1",             // unique-local
        "2001:db8::1",         // documentation
        "::ffff:192.168.1.13", // IPv4 in disguise
        "::",                  // unspecified
        "ff02::1",             // multicast
    ] {
        let a: std::net::Ipv6Addr = local.parse().unwrap();
        assert!(!is_global_ipv6(a), "{local} must not be announced");
    }
}

/// TCP relay that delays every byte by `one_way` in each direction, without limiting throughput:
/// real latency as seen by TCP and TLS, simulated in user space (no `tc` without privileges).
async fn latency_proxy(target: std::net::SocketAddr, one_way: std::time::Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((inbound, _)) = listener.accept().await else {
                return;
            };
            let _ = inbound.set_nodelay(true);
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let _ = outbound.set_nodelay(true);
                let (ri, wi) = inbound.into_split();
                let (ro, wo) = outbound.into_split();
                // One direction: read, timestamp, and write only when due; order is preserved.
                let pump = |mut r: tokio::net::tcp::OwnedReadHalf,
                            mut w: tokio::net::tcp::OwnedWriteHalf| async move {
                    let (tx, mut rx) =
                        tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
                    let writer = tokio::spawn(async move {
                        while let Some((due, bytes)) = rx.recv().await {
                            tokio::time::sleep_until(due).await;
                            if w.write_all(&bytes).await.is_err() {
                                return;
                            }
                        }
                        let _ = w.shutdown().await;
                    });
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match r.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let due = tokio::time::Instant::now() + one_way;
                                if tx.send((due, buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    drop(tx);
                    let _ = writer.await;
                };
                tokio::join!(pump(ri, wo), pump(ro, wi));
            });
        }
    });
    addr
}

/// One connection reused for every chunk of a transfer, against a fresh connection per chunk,
/// behind a 40 ms round trip. Each fresh connection pays a TCP handshake and a TLS 1.3 handshake
/// before its request: three round trips per chunk instead of one.
#[tokio::test]
async fn reusing_the_connection_saves_two_round_trips_per_chunk() {
    let chunks = 8u32;
    let f = fixture(chunks as usize * CHUNK as usize);
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        4,
        1 << 30,
    )
    .await
    .unwrap();
    let proxy = latency_proxy(holder.addr(), std::time::Duration::from_millis(20)).await;
    let fp = holder.fingerprint();
    let timeout = std::time::Duration::from_secs(10);
    let ticket = ticket_for(
        &f.server_key,
        f.hash,
        f.data.len() as u64,
        &f.reader,
        60_000,
    );
    let path = |i: u32| format!("/p2p/v1/artifacts/{}/chunks/{i}", f.hash);

    // Before: one connection per chunk.
    let t0 = std::time::Instant::now();
    for i in 0..chunks {
        let headers = headers_for(&ticket, &f.reader, f.hash, i, UnixMillis::now());
        let (status, body) = peer_get(&proxy, fp, &path(i), &headers, CHUNK as usize, timeout)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), CHUNK as usize);
    }
    let per_chunk = t0.elapsed();

    // Now: one session for the whole transfer.
    let t0 = std::time::Instant::now();
    let mut session = gg_worker::peer_tls::PeerSession::connect(&proxy, fp, timeout)
        .await
        .unwrap();
    for i in 0..chunks {
        let headers = headers_for(&ticket, &f.reader, f.hash, i, UnixMillis::now());
        let (status, body) = session
            .get(&path(i), &headers, CHUNK as usize, timeout)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.len(), CHUNK as usize);
    }
    let reused = t0.elapsed();
    eprintln!(
        "[peers] {chunks} chunks behind a 40 ms round trip: {} ms with one connection per chunk, \
{} ms with a single one",
        per_chunk.as_millis(),
        reused.as_millis()
    );
    assert!(
        reused.as_secs_f64() < 0.6 * per_chunk.as_secs_f64(),
        "{reused:?} vs {per_chunk:?}"
    );
}

/// Several chunks in flight rather than one round trip per chunk: the same 16-chunk transfer
/// behind a 40 ms round trip, one at a time and then four at a time. The resulting file is the
/// same, verified chunk by chunk and then as a whole.
#[tokio::test]
async fn several_chunks_in_flight_fill_the_link_instead_of_waiting_a_round_trip_each() {
    let chunks = 16usize;
    let f = fixture(chunks * CHUNK as usize);
    let holder = PeerServer::start(
        "127.0.0.1:0",
        f.cache.clone(),
        net(),
        f.server_key.public_key(),
        8,
        1 << 30,
    )
    .await
    .unwrap();
    let proxy = latency_proxy(holder.addr(), std::time::Duration::from_millis(20)).await;
    let cp = ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        peers: Arc::new(vec![(proxy.clone(), holder.fingerprint())]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(f.reader.public_key()),
    };
    let base = start_control_plane(cp.clone()).await;
    let api = Api::new(&base).unwrap();
    // Best of up to three paired measurements, same threshold: the tests of this file run
    // concurrently, and on a two-core CI runner their CPU use once stalled the four-at-a-time
    // transfer (739 ms one at a time, 801 ms four at a time; the rerun passed).
    let mut best = [std::time::Duration::MAX; 2];
    for _attempt in 0..3 {
        for (slot, parallel) in [1usize, 4].into_iter().enumerate() {
            let out = tempfile::tempdir().unwrap();
            let dest = out.path().join("artifact");
            let t0 = std::time::Instant::now();
            let stats = fetch_with_peers(
                &api,
                "token",
                &f.reader,
                &net(),
                &f.server_key.public_key(),
                &ArtifactRef {
                    hash: f.hash,
                    size: f.data.len() as u64,
                },
                &format!("/worker/artifacts/{}", f.hash),
                &dest,
                &FetchConfig {
                    parallel_chunks: parallel,
                    ..cfg()
                },
            )
            .await
            .unwrap();
            best[slot] = best[slot].min(t0.elapsed());
            assert_eq!(std::fs::read(&dest).unwrap(), f.data);
            assert_eq!(stats.chunks_from_peers, chunks as u32);
            assert!(stats.peer_connections as usize <= parallel);
        }
        if best[1].as_secs_f64() < 0.6 * best[0].as_secs_f64() {
            break;
        }
    }
    eprintln!(
        "[peers] {chunks} chunks behind a 40 ms round trip: {} ms one at a time, {} ms four at a \
time",
        best[0].as_millis(),
        best[1].as_millis()
    );
    assert!(
        best[1].as_secs_f64() < 0.6 * best[0].as_secs_f64(),
        "{best:?}"
    );
    assert_eq!(cp.ranges_served.load(Ordering::SeqCst), 0);
}

/// Swarm: two readers start together on the same 16-chunk artifact, with no complete holder. Each
/// starts at a different place and serves what it has already verified, so they exchange chunks
/// instead of asking the control server for everything (placed behind a 40 ms round trip). Both
/// files are exact.
#[tokio::test]
async fn readers_that_start_together_exchange_chunks_instead_of_all_asking_the_server() {
    let chunks = 16usize;
    let f = fixture(chunks * CHUNK as usize);
    let reader_b = SecretKey::generate().unwrap();
    let serve = |cache: ArtifactCache| {
        let key = f.server_key.public_key();
        async move {
            PeerServer::start("127.0.0.1:0", cache, net(), key, 8, 1 << 30)
                .await
                .unwrap()
        }
    };
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let srv_a = serve(ArtifactCache::new(dir_a.path().join("cache"), 1 << 30)).await;
    let srv_b = serve(ArtifactCache::new(dir_b.path().join("cache"), 1 << 30)).await;
    let control_plane = |reader: &SecretKey, other: &PeerServer| ControlPlane {
        data: Arc::new(f.data.clone()),
        hash: f.hash,
        key: f.server_key.clone(),
        peers: Arc::new(vec![(other.addr().to_string(), other.fingerprint())]),
        ranges_served: Arc::new(AtomicUsize::new(0)),
        reader_key: Arc::new(reader.public_key()),
    };
    let (cp_a, cp_b) = (
        control_plane(&f.reader, &srv_b),
        control_plane(&reader_b, &srv_a),
    );
    let far = |base: String| async move {
        let addr: std::net::SocketAddr = base.trim_start_matches("http://").parse().unwrap();
        format!(
            "http://{}",
            latency_proxy(addr, std::time::Duration::from_millis(20)).await
        )
    };
    let base_a = far(start_control_plane(cp_a.clone()).await).await;
    let base_b = far(start_control_plane(cp_b.clone()).await).await;
    let api_a = Api::new(&base_a).unwrap();
    let api_b = Api::new(&base_b).unwrap();
    let artifact = ArtifactRef {
        hash: f.hash,
        size: f.data.len() as u64,
    };
    let path = format!("/worker/artifacts/{}", f.hash);
    let (dest_a, dest_b) = (dir_a.path().join("a"), dir_b.path().join("b"));
    let cfg_a = FetchConfig {
        partials: Some(srv_a.partials()),
        self_fingerprint: Some(srv_a.fingerprint()),
        ..cfg()
    };
    let cfg_b = FetchConfig {
        partials: Some(srv_b.partials()),
        self_fingerprint: Some(srv_b.fingerprint()),
        ..cfg()
    };
    let server_key = f.server_key.public_key();
    let network = net();
    let t0 = std::time::Instant::now();
    let (a, b) = tokio::join!(
        fetch_with_peers(
            &api_a,
            "t",
            &f.reader,
            &network,
            &server_key,
            &artifact,
            &path,
            &dest_a,
            &cfg_a
        ),
        fetch_with_peers(
            &api_b,
            "t",
            &reader_b,
            &network,
            &server_key,
            &artifact,
            &path,
            &dest_b,
            &cfg_b
        ),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(std::fs::read(&dest_a).unwrap(), f.data);
    assert_eq!(std::fs::read(&dest_b).unwrap(), f.data);
    let from_server =
        cp_a.ranges_served.load(Ordering::SeqCst) + cp_b.ranges_served.load(Ordering::SeqCst);
    eprintln!(
        "[swarm] 2 readers × {chunks} chunks: {from_server} requested from the server, {} exchanged \
between them (A {} + B {}), {} ms",
        a.chunks_from_peers + b.chunks_from_peers,
        a.chunks_from_peers,
        b.chunks_from_peers,
        t0.elapsed().as_millis()
    );
    assert_eq!(a.bad_chunks + b.bad_chunks, 0);
    assert_eq!(
        from_server + (a.chunks_from_peers + b.chunks_from_peers) as usize,
        2 * chunks
    );
    // Ideal: a single copy (16). Each reader asks the server only for the chunks it is
    // responsible for; a peer list not yet up to date at startup costs a few more.
    assert!(
        from_server <= chunks + chunks / 4,
        "{from_server} chunks requested from the server for two readers of {chunks}"
    );
}
