//! Network client logic that does not depend on a transport: URL parsing,
//! HTTP/1.0 requests and responses, SNTP packets, and a TLS client that
//! turns encrypted bytes into plaintext and back. The kernel drives it from smoltcp
//! sockets; the simulator from `std::net`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod http;
pub mod sntp;
pub mod tls;
