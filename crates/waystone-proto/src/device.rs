//! Device identities and the small value types they carry over the wire.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Stable identifier of a device.
///
/// It is derived from the device public key so a device can never claim an
/// identity it does not own: `hex(sha256(public_key))[..16]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(String);

impl DeviceId {
    /// Wraps an already formatted identifier.
    pub fn new(id: impl Into<String>) -> Self {
        DeviceId(id.into())
    }

    /// Derives the identifier from a 32 byte Ed25519 public key.
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(public_key);
        let mut hexed = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            hexed.push_str(&format!("{byte:02x}"));
        }
        DeviceId(hexed)
    }

    /// Borrows the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for DeviceId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(DeviceId(s.to_string()))
    }
}

/// What a device is allowed to do inside the private pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Machine that streams its desktop (the "controlled" side).
    Host,
    /// Phone, tablet or desktop that views and drives a host.
    Controller,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Host => "host",
            Role::Controller => "controller",
        })
    }
}

impl FromStr for Role {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "host" => Role::Host,
            _ => Role::Controller,
        })
    }
}

/// Operating system family of a device, used for UI hints only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Macos,
    Linux,
    Ios,
    Ipados,
    Android,
    Other,
}

impl Platform {
    /// Human readable label shown in the device list.
    pub fn label(&self) -> &'static str {
        match self {
            Platform::Windows => "Windows",
            Platform::Macos => "macOS",
            Platform::Linux => "Linux",
            Platform::Ios => "iOS",
            Platform::Ipados => "iPadOS",
            Platform::Android => "Android",
            Platform::Other => "Unknown",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Windows => "windows",
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Ios => "ios",
            Platform::Ipados => "ipados",
            Platform::Android => "android",
            Platform::Other => "other",
        })
    }
}

impl FromStr for Platform {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "windows" => Platform::Windows,
            "macos" => Platform::Macos,
            "linux" => Platform::Linux,
            "ios" => Platform::Ios,
            "ipados" => Platform::Ipados,
            "android" => Platform::Android,
            _ => Platform::Other,
        })
    }
}

/// Video codec the host encodes and the controller decodes.
///
/// The host encodes, but only the controller knows what it can decode (an iOS
/// simulator has no HEVC decoder, for instance), so the controller names the
/// codec in `SessionStart` and the server echoes it to both ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    #[serde(rename = "h264")]
    H264,
    #[default]
    #[serde(rename = "h265")]
    H265,
}

impl VideoCodec {
    /// Wire name, identical to the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
        }
    }
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VideoCodec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(VideoCodec::H264),
            "h265" | "hevc" => Ok(VideoCodec::H265),
            other => Err(format!("unknown video codec {other}")),
        }
    }
}

/// Public description of a paired device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Derived identifier, see [`DeviceId::from_public_key`].
    pub id: DeviceId,
    pub name: String,
    pub platform: Platform,
    pub role: Role,
    /// Base64 encoded 32 byte Ed25519 public key.
    pub public_key: String,
}

/// Controller viewport reported when a session starts.
///
/// The host uses it to pick a capture region that fills the phone screen
/// without letterboxing.
///
/// All values are integers so the struct can travel through `Eq` containers
/// and be logged without floating point formatting differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    /// Width in device pixels, e.g. `2556` for a landscape iPhone 15 Pro Max.
    pub width: u32,
    /// Height in device pixels, e.g. `1179`.
    pub height: u32,
    /// Native pixels per inch, used for cursor scaling hints; `0` when unknown.
    pub dpi: u16,
    /// Requested frame rate cap: `60` for smooth, `45` for low power.
    pub fps: u32,
}

impl Viewport {
    /// Aspect ratio `width / height`, guarding against a zero height.
    pub fn aspect(&self) -> f32 {
        if self.height == 0 {
            16.0 / 9.0
        } else {
            self.width as f32 / self.height as f32
        }
    }

    /// Clamps the requested frame rate into the two supported tiers.
    ///
    /// Anything at or above 50 is treated as the smooth tier so that a device
    /// reporting a 120 Hz display still gets a predictable 60 fps stream.
    pub fn frame_rate_tier(&self) -> u32 {
        if self.fps >= 50 {
            60
        } else {
            45
        }
    }
}
