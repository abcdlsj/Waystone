//! WSS signaling messages.
//!
//! One JSON object per WebSocket text frame, tagged by `type`. The set of
//! messages is intentionally tiny: everything that can be expressed as a state
//! change instead of a request/response is pushed by the server.
//!
//! Flow for a brand new phone:
//!
//! ```text
//! client -> PairRequest  (pairing token from the QR code)
//! client <- PairAccepted (server remembers the device public key)
//! client <- Challenge    (per connection nonce)
//! client -> Auth         (signature over the nonce)
//! client <- AuthOk       (device list plus online state)
//! ```
//!
//! Flow for an established device:
//!
//! ```text
//! client <- Challenge ; client -> Auth ; client <- AuthOk
//! client -> SessionStart ; client <- SessionAccepted
//! client <-> RtcSignal (SDP offer/answer, trickle ICE)
//! client -> SessionBye   (or the host decides first)
//! ```

use serde::{Deserialize, Serialize};

use crate::device::{DeviceId, DeviceInfo, Viewport};
use crate::identity::Identity;
use crate::PROTOCOL_VERSION;

/// An SDP payload or an ICE candidate, relayed verbatim between peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RtcSignal {
    /// Controller-to-host SDP offer.
    Offer { sdp: String },
    /// Host-to-controller SDP answer.
    Answer { sdp: String },
    /// Trickled ICE candidate.
    Ice {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    /// Everything the sender wanted to trickle has been sent.
    IceComplete,
}

/// Runtime tweaks while a session is live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionControl {
    /// Switch frame rate cap, `45` for low power or `60` for smooth.
    SetFrameRate { fps: u32 },
    /// Ask the host to emit an IDR frame right now.
    RequestKeyframe,
    /// Pause or resume the video track without tearing the session down.
    SetVideoEnabled { enabled: bool },
    /// Mute or unmute the host's captured audio.
    SetAudioEnabled { enabled: bool },
    /// Switch the captured display by index.
    SelectDisplay { index: u32 },
}

/// Messages a device sends to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Enroll a device using a single use pairing token.
    PairRequest { token: String, device: DeviceInfo },
    /// Answer the server challenge with an Ed25519 signature.
    Auth {
        identity: Identity,
        /// Base64 signature over [`crate::identity::Challenge::signing_payload`].
        signature: String,
        protocol_version: u32,
        /// Advertised display name, refreshed on every connect.
        name: String,
    },
    /// Ask the server to open a session towards another device.
    SessionStart {
        target: DeviceId,
        viewport: Viewport,
        audio: bool,
    },
    /// Relay an SDP/ICE message to the peer of an existing session.
    RtcSignal {
        session_id: String,
        signal: RtcSignal,
    },
    /// Runtime tweaks for an existing session.
    SessionControl {
        session_id: String,
        control: SessionControl,
    },
    /// Tear the session down; the host performs its rollback sequence.
    SessionBye {
        session_id: String,
        reason: ByeReason,
    },
    /// Periodic latency probe used to render the connection capsule.
    Ping { nonce: u64 },
}

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// The user pressed hang up.
    UserHangup,
    /// The user pressed hang up and the host should lock the session.
    LockAndExit,
    /// The media path did not recover within the reconnection window.
    NetworkTimeout,
    /// The host side failed, for example the capture process died.
    HostError,
}

/// Short lived TURN credentials handed out after authentication.
///
/// The daemon derives them from a shared secret instead of storing per-device
/// passwords, and hands the same pair to both ends of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredentials {
    /// STUN/TURN authority the clients must use, e.g. `home.example.com:3478`.
    pub server: String,
    pub username: String,
    pub password: String,
}

/// Online state of a paired host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerState {
    pub device: DeviceInfo,
    /// True while the host keeps its signaling connection open.
    pub online: bool,
    /// Round trip time of the server probe, in milliseconds.
    pub rtt_ms: Option<u32>,
    /// Display name of the host machine, e.g. `DESKTOP-4F2`.
    pub hostname: Option<String>,
}

/// Messages the server sends to a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent immediately after the WebSocket handshake.
    Challenge {
        nonce: String,
        server_id: String,
        protocol_version: u32,
    },
    /// The device is now enrolled; it must still authenticate.
    PairAccepted { server_id: String },
    /// Pairing failed, for example because the token expired.
    PairRejected { reason: String },
    /// Authentication succeeded; the payload is the current device list.
    AuthOk {
        peers: Vec<PeerState>,
        /// Relay credentials, omitted when the instance has no TURN server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<TurnCredentials>,
    },
    /// Authentication failed and the socket will be closed.
    AuthRejected { reason: String },
    /// Somebody else in the pool came online or went offline.
    PeerUpdate { peers: Vec<PeerState> },
    /// A controller asked to connect; both ends receive the same id.
    SessionAccepted {
        session_id: String,
        /// `true` for the host side, `false` for the controller side.
        is_host: bool,
        peer: DeviceInfo,
        viewport: Option<Viewport>,
        audio: bool,
    },
    /// The requested peer cannot accept a session right now.
    SessionRejected { reason: String },
    /// Relayed SDP/ICE message from the session peer.
    RtcSignal {
        session_id: String,
        signal: RtcSignal,
    },
    /// Relayed runtime tweak from the session peer.
    SessionControl {
        session_id: String,
        control: SessionControl,
    },
    /// The session ended; carries the cause so the UI can explain it.
    SessionEnded {
        session_id: String,
        reason: ByeReason,
        /// Free form detail, for example `host went offline`.
        detail: Option<String>,
    },
    /// Reply to [`ClientMessage::Ping`].
    Pong { nonce: u64 },
    /// Non fatal protocol error.
    Error { message: String },
}

impl ClientMessage {
    /// Serializes to a JSON text frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signal message is always serializable")
    }

    /// Parses a JSON text frame.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Protocol version advertised by the sender, if the message carries one.
    pub fn advertised_version(&self) -> Option<u32> {
        match self {
            ClientMessage::Auth {
                protocol_version, ..
            } => Some(*protocol_version),
            _ => None,
        }
    }
}

impl ServerMessage {
    /// Serializes to a JSON text frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signal message is always serializable")
    }

    /// Parses a JSON text frame.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// True when a peer speaking `version` can talk to us.
///
/// Only the major part matters for the MVP, and it is the whole number today.
pub fn version_compatible(version: u32) -> bool {
    version == PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Platform, Role};

    fn controller() -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new("0123456789abcdef"),
            name: "iPhone".into(),
            platform: Platform::Ios,
            role: Role::Controller,
            public_key: crate::identity::encode_public_key(&[1u8; 32]),
        }
    }

    #[test]
    fn client_message_roundtrip_is_tagged() {
        let message = ClientMessage::SessionStart {
            target: DeviceId::new("host"),
            viewport: Viewport {
                width: 2556,
                height: 1179,
                dpi: 460,
                fps: 120,
            },
            audio: true,
        };
        let json = message.to_json();
        assert!(json.contains("\"type\":\"session_start\""));
        assert_eq!(ClientMessage::from_json(&json).unwrap(), message);
    }

    #[test]
    fn frame_rate_tier_clamps_to_supported_values() {
        let base = Viewport {
            width: 1,
            height: 1,
            dpi: 0,
            fps: 0,
        };
        assert_eq!(Viewport { fps: 45, ..base }.frame_rate_tier(), 45);
        assert_eq!(Viewport { fps: 30, ..base }.frame_rate_tier(), 45);
        assert_eq!(Viewport { fps: 60, ..base }.frame_rate_tier(), 60);
        assert_eq!(Viewport { fps: 120, ..base }.frame_rate_tier(), 60);
    }

    #[test]
    fn rtc_signal_roundtrip() {
        let signal = RtcSignal::Ice {
            candidate: "candidate:1 1 udp 2130706431 10.0.0.2 54321 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        };
        let json = serde_json::to_string(&signal).unwrap();
        assert_eq!(serde_json::from_str::<RtcSignal>(&json).unwrap(), signal);
    }

    #[test]
    fn server_message_roundtrip() {
        let message = ServerMessage::PeerUpdate {
            peers: vec![PeerState {
                device: controller(),
                online: true,
                rtt_ms: Some(12),
                hostname: Some("Living room".into()),
            }],
        };
        let json = message.to_json();
        assert!(json.contains("\"type\":\"peer_update\""));
        assert_eq!(ServerMessage::from_json(&json).unwrap(), message);
    }

    #[test]
    fn version_gate() {
        assert!(version_compatible(PROTOCOL_VERSION));
        assert!(!version_compatible(PROTOCOL_VERSION + 1));
    }
}
