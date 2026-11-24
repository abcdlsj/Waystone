//! The Waystone self-hosted daemon.
//!
//! One binary serves three things on two ports:
//!
//! * the admin panel and the pairing QR code over HTTPS on `TCP 8443`,
//! * the JSON signaling channel over WSS on the same port,
//! * STUN and TURN on `UDP 3478`.
//!
//! The library crate exists so the hub, store and pairing logic can be tested
//! without binding sockets; the binary in `main.rs` only wires them together.

pub mod auth;
pub mod challenge;
pub mod http;
pub mod hub;
pub mod instance;
pub mod lan;
pub mod pairing;
pub mod store;
pub mod tls;
pub mod turnserver;

pub use hub::Hub;
pub use instance::Instance;
pub use store::Store;
