//! Contributor agent: talks to the control plane over HTTPS, manages the key, cache and
//! supervision; executes compute in a separate `gg-executor` process with typed arguments.

#![deny(unsafe_code)]

pub mod agent;
#[cfg(windows)]
#[allow(unsafe_code)] // CreateProcessW with the sandbox's attributes, each call documented
pub mod appcontainer;
pub mod bundle;
pub mod config;
pub mod control;
pub mod doctor;
pub mod kernel_cache;
pub mod launch;
// Peer-to-peer exchange and router port mapping live in gg-client (shared with the lab node,
// ADR-0041); re-exported under their former paths.
pub use gg_client::{nat_probe, natpmp, p2p, peer_tls, upnp};
#[allow(unsafe_code)] // libc calls for rlimits/getpid/socket probes, each documented
pub mod restrict;
pub mod sandbox;
pub mod session;
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 calls for the owner's policy readings, each documented
pub mod winsys;
