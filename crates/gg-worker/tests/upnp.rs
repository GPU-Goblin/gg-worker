//! UPnP-IGD client against a gateway that speaks SOAP (ADR-0013). The exchange is real HTTP: what is
//! simulated is the router, not the client. Measured against a real home router on 2026-09-17;
//! this test keeps the behaviour pinned without one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use gg_worker::upnp::{Gateway, UpnpError};

#[derive(Clone, Default)]
struct FakeIgd {
    /// Mapped external port, if any.
    mapping: Arc<std::sync::Mutex<Option<(u16, SocketAddr, u32)>>>,
    calls: Arc<AtomicUsize>,
}

fn envelope(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>{inner}</s:Body></s:Envelope>"
    )
}

fn fault(code: u16, description: &str) -> String {
    envelope(&format!(
        "<s:Fault><faultcode>s:Client</faultcode><detail><UPnPError><errorCode>{code}</errorCode><errorDescription>{description}</errorDescription></UPnPError></detail></s:Fault>"
    ))
}

fn field(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].to_owned())
}

async fn control(State(igd): State<FakeIgd>, body: String) -> axum::response::Response {
    igd.calls.fetch_add(1, Ordering::SeqCst);
    let xml = if body.contains("GetExternalIPAddress") {
        envelope(
            "<u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.42</NewExternalIPAddress></u:GetExternalIPAddressResponse>",
        )
    } else if body.contains("AddPortMapping") {
        let external: u16 = field(&body, "NewExternalPort")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let internal_port: u16 = field(&body, "NewInternalPort")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let client: Ipv4Addr = field(&body, "NewInternalClient")
            .and_then(|v| v.parse().ok())
            .unwrap_or(Ipv4Addr::UNSPECIFIED);
        let lease: u32 = field(&body, "NewLeaseDuration")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // A router refuses a conflicting entry rather than silently stealing it.
        let taken = igd
            .mapping
            .lock()
            .unwrap()
            .is_some_and(|(p, c, _)| p == external && c.ip() != IpAddr::V4(client));
        if taken {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                fault(718, "ConflictInMappingEntry"),
            )
                .into_response();
        }
        *igd.mapping.lock().unwrap() = Some((
            external,
            SocketAddr::from((IpAddr::V4(client), internal_port)),
            lease,
        ));
        envelope("<u:AddPortMappingResponse></u:AddPortMappingResponse>")
    } else if body.contains("GetSpecificPortMappingEntry") {
        let external: u16 = field(&body, "NewExternalPort")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match *igd.mapping.lock().unwrap() {
            Some((p, client, lease)) if p == external => envelope(&format!(
                "<u:GetSpecificPortMappingEntryResponse><NewInternalPort>{}</NewInternalPort><NewInternalClient>{}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>gpu-goblin</NewPortMappingDescription><NewLeaseDuration>{lease}</NewLeaseDuration></u:GetSpecificPortMappingEntryResponse>",
                client.port(),
                client.ip()
            )),
            _ => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    fault(714, "NoSuchEntryInArray"),
                )
                    .into_response();
            }
        }
    } else if body.contains("DeletePortMapping") {
        let external: u16 = field(&body, "NewExternalPort")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut m = igd.mapping.lock().unwrap();
        if m.is_some_and(|(p, _, _)| p == external) {
            *m = None;
            envelope("<u:DeletePortMappingResponse></u:DeletePortMappingResponse>")
        } else {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                fault(714, "NoSuchEntryInArray"),
            )
                .into_response();
        }
    } else {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            fault(401, "InvalidAction"),
        )
            .into_response();
    };
    ([("content-type", "text/xml")], xml).into_response()
}

async fn start_igd() -> (Gateway, FakeIgd) {
    let igd = FakeIgd::default();
    let app = Router::new()
        .route("/upnp/control/WANIPConn1", post(control))
        .with_state(igd.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        Gateway {
            control_url: format!("http://{addr}/upnp/control/WANIPConn1"),
            service_type: "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
            address: addr.ip(),
        },
        igd,
    )
}

#[tokio::test]
async fn a_mapping_is_created_read_back_and_deleted() {
    let (gw, igd) = start_igd().await;
    assert_eq!(
        gw.external_address().await.unwrap(),
        Ipv4Addr::new(203, 0, 113, 42)
    );
    let internal = SocketAddr::from((IpAddr::V4(Ipv4Addr::new(192, 168, 1, 13)), 47654));
    assert!(gw.get_port_mapping(47654).await.unwrap().is_none());
    gw.add_port_mapping(internal, 47654, 120, "gpu-goblin peer")
        .await
        .unwrap();
    // The gateway's own table is what proves the mapping, not our request.
    let (client, lease) = gw.get_port_mapping(47654).await.unwrap().unwrap();
    assert_eq!(client, internal);
    assert_eq!(lease, 120, "a mapping is leased, never permanent");
    gw.delete_port_mapping(47654).await.unwrap();
    assert!(
        gw.get_port_mapping(47654).await.unwrap().is_none(),
        "the gateway confirms the port is closed again"
    );
    assert!(igd.calls.load(Ordering::SeqCst) >= 5);
}

#[tokio::test]
async fn a_refusal_is_reported_with_its_upnp_code() {
    let (gw, igd) = start_igd().await;
    // Someone else already holds the external port.
    *igd.mapping.lock().unwrap() = Some((
        47654,
        SocketAddr::from((IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99)), 47654)),
        3600,
    ));
    let internal = SocketAddr::from((IpAddr::V4(Ipv4Addr::new(192, 168, 1, 13)), 47654));
    let err = gw
        .add_port_mapping(internal, 47654, 120, "gpu-goblin peer")
        .await
        .unwrap_err();
    match err {
        UpnpError::Refused { code, description } => {
            assert_eq!(code, 718);
            assert_eq!(description, "ConflictInMappingEntry");
        }
        other => panic!("expected a refusal, got {other}"),
    }
    // Deleting an entry that is not ours is refused too, and says so.
    let err = gw.delete_port_mapping(5000).await.unwrap_err();
    assert!(matches!(err, UpnpError::Refused { code: 714, .. }), "{err}");
}

#[tokio::test]
async fn an_unreachable_gateway_fails_quickly_instead_of_hanging() {
    let gw = Gateway {
        // Nothing listens there.
        control_url: "http://127.0.0.1:9/upnp/control/WANIPConn1".into(),
        service_type: "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let started = std::time::Instant::now();
    let err = gw.external_address().await.unwrap_err();
    assert!(matches!(err, UpnpError::Http(_)), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}
