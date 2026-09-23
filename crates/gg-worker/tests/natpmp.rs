//! NAT-PMP client against a gateway that speaks the protocol (RFC 6886). The exchange is real UDP:
//! what is simulated is the router, not the client. A worker uses this to obtain a port mapping
//! before announcing itself; every failure path must leave peer sharing working through the address
//! the control plane observed (ADR-0013).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gg_worker::natpmp::{self, NatPmpError};
use tokio::net::UdpSocket;

/// A gateway that answers like a home router. `refuse` makes it answer with a result code.
async fn fake_gateway(
    external: Ipv4Addr,
    external_port: u16,
    refuse: Option<u16>,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 32];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            if n < 2 || buf[0] != 0 {
                continue;
            }
            let answer: Vec<u8> = match buf[1] {
                // External address request.
                0 => {
                    let mut a = vec![0u8; 12];
                    a[1] = 128;
                    a[4..8].copy_from_slice(&1_234_567u32.to_be_bytes()); // seconds since start
                    a[8..12].copy_from_slice(&external.octets());
                    a
                }
                // TCP mapping request.
                2 => {
                    let internal = u16::from_be_bytes([buf[4], buf[5]]);
                    let lifetime = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                    let mut a = vec![0u8; 16];
                    a[1] = 2 + 128;
                    if let Some(code) = refuse {
                        a[2..4].copy_from_slice(&code.to_be_bytes());
                    }
                    a[4..8].copy_from_slice(&1_234_567u32.to_be_bytes());
                    a[8..10].copy_from_slice(&internal.to_be_bytes());
                    a[10..12].copy_from_slice(&external_port.to_be_bytes());
                    a[12..16].copy_from_slice(&lifetime.to_be_bytes());
                    a
                }
                _ => continue,
            };
            let _ = socket.send_to(&answer, from).await;
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn a_gateway_that_answers_gives_the_address_to_announce() {
    let (gateway, seen) = fake_gateway(Ipv4Addr::new(203, 0, 113, 42), 41234, None).await;
    let (addr, mapping) = natpmp::map_port(7000, 3600, Duration::from_secs(2), Some(gateway))
        .await
        .unwrap();
    assert_eq!(addr.to_string(), "203.0.113.42:41234");
    assert_eq!(mapping.internal_port, 7000);
    assert_eq!(mapping.external_port, 41234);
    assert_eq!(mapping.lifetime_secs, 3600);
    // One address request and one mapping request.
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_gateway_that_refuses_is_reported_rather_than_guessed_around() {
    // Result code 2 = network failure (RFC 6886 §3.5).
    let (gateway, _) = fake_gateway(Ipv4Addr::new(203, 0, 113, 42), 41234, Some(2)).await;
    let err = natpmp::map_port(7000, 3600, Duration::from_secs(2), Some(gateway))
        .await
        .unwrap_err();
    assert!(matches!(err, NatPmpError::Refused(2)), "{err}");
}

#[tokio::test]
async fn silence_from_the_gateway_ends_in_a_bounded_failure() {
    // A socket nobody reads from: the client must give up quickly instead of hanging.
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let gateway = silent.local_addr().unwrap();
    let started = std::time::Instant::now();
    let err = natpmp::map_port(7000, 3600, Duration::from_millis(200), Some(gateway))
        .await
        .unwrap_err();
    assert!(matches!(err, NatPmpError::NoAnswer(_)), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the client waited {:?}",
        started.elapsed()
    );
}
