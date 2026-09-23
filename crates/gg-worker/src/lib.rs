//! Contributor agent: talks to the control plane over HTTPS, manages the key, cache and
//! supervision; executes compute in a separate `gg-executor` process with typed arguments.

#![deny(unsafe_code)]

pub mod agent;
pub mod bundle;
pub mod config;
pub mod doctor;
pub mod kernel_cache;
pub mod nat_probe;
pub mod natpmp;
pub mod p2p;
pub mod peer_tls;
#[allow(unsafe_code)] // libc calls for rlimits/getpid/socket probes, each documented
pub mod restrict;
pub mod sandbox;
pub mod session;
pub mod upnp;
