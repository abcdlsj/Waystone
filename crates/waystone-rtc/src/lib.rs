//! Shared WebRTC transport for Waystone hosts and controllers.
//!
//! The crate is used in two ways:
//!
//! * Directly as a Rust library (the macOS host agent).
//! * Through the C ABI in [`ffi`] (the iOS controller app links it as a static
//!   library, so both ends run the exact same ICE, DTLS and RTP code).
//!
//! All `Rtc` mutations happen inside a single driver task because str0m requires
//! that every mutation is followed by draining `poll_output`. Callers talk to
//! that task through a command channel and receive results through a callback.

pub mod engine;
pub mod ffi;
pub mod net;
#[cfg(feature = "port-mapping")]
mod portmap;
pub mod stun;
pub mod turn;

pub use engine::{Engine, EngineConfig, Event, Role, SessionOptions, VideoCodec};
