//! Wire protocol shared by every Waystone component.
//!
//! The crate is deliberately dependency-light so that it can be compiled for
//! desktop hosts, the self-hosted server and mobile controllers alike.
//!
//! * [`signal`] – JSON messages exchanged over the WSS signaling channel.
//! * [`input`] – compact binary frames exchanged over the WebRTC data channels.
//! * [`pairing`] – the `waystone://pair?...` URI rendered as a QR code.
//! * [`identity`] – Ed25519 device identities and challenge signatures.

pub mod device;
pub mod identity;
pub mod input;
pub mod pairing;
pub mod signal;

pub use device::{DeviceId, DeviceInfo, Platform, Role, Viewport};
pub use pairing::PairingUri;
pub use signal::{ClientMessage, RtcSignal, ServerMessage, SessionControl};

/// Protocol version advertised during the handshake.
///
/// Peers with a different major version are rejected with an explicit error
/// instead of failing later in the media pipeline.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port serving both the admin panel and the WSS signaling channel.
pub const DEFAULT_HTTPS_PORT: u16 = 8443;

/// Default UDP port serving STUN and TURN.
pub const DEFAULT_TURN_PORT: u16 = 3478;
