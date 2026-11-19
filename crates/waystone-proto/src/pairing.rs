//! The `waystone://pair?...` URI.
//!
//! A fresh instance renders this URI as a QR code. Scanning it is the only
//! manual step required to enroll a phone, and every value it carries is
//! designed so the client never has to type an address, port or key.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Scheme and host part of every pairing URI.
pub const PAIRING_SCHEME: &str = "waystone://pair";

/// Parsed pairing payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingUri {
    /// Server authority as reachable from the phone, e.g. `home.example.com:8443`.
    pub endpoint: String,
    /// STUN/TURN authority, e.g. `home.example.com:3478`.
    pub turn: String,
    /// SHA-256 fingerprint of the server certificate, lowercase hex, no colons.
    ///
    /// Clients pin it, which is what makes self-signed certificates usable
    /// without ever showing a scary warning.
    pub fingerprint: String,
    /// Single use enrollment token, valid for 10 minutes.
    pub token: String,
}

/// Why a pairing URI could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    WrongScheme,
    MissingField(&'static str),
    EmptyField(&'static str),
    BadToken,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairingError::WrongScheme => f.write_str("not a waystone://pair URI"),
            PairingError::MissingField(name) => write!(f, "pairing URI is missing `{name}`"),
            PairingError::EmptyField(name) => write!(f, "pairing URI field `{name}` is empty"),
            PairingError::BadToken => f.write_str("pairing token has an unexpected shape"),
        }
    }
}

impl std::error::Error for PairingError {}

impl PairingUri {
    /// Renders the compact single line form placed in the QR code.
    pub fn to_uri(&self) -> String {
        format!(
            "{PAIRING_SCHEME}?ep={}&turn={}&fp={}&tk={}",
            percent_encode(&self.endpoint),
            percent_encode(&self.turn),
            self.fingerprint,
            self.token
        )
    }

    /// True when the fingerprint looks like a SHA-256 in lowercase hex.
    pub fn fingerprint_is_well_formed(&self) -> bool {
        self.fingerprint.len() == 64
            && self
                .fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }
}

impl fmt::Display for PairingUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri())
    }
}

impl FromStr for PairingUri {
    type Err = PairingError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        let rest = trimmed
            .strip_prefix(PAIRING_SCHEME)
            .ok_or(PairingError::WrongScheme)?;
        let rest = rest
            .strip_prefix('?')
            .ok_or(PairingError::MissingField("query"))?;

        let mut endpoint = None;
        let mut turn = None;
        let mut fingerprint = None;
        let mut token = None;

        for pair in rest.split('&').filter(|s| !s.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            let value = percent_decode(value);
            if value.is_empty() {
                // Keep the field name for a precise error message below.
                match key {
                    "ep" => return Err(PairingError::EmptyField("ep")),
                    "turn" => return Err(PairingError::EmptyField("turn")),
                    "fp" => return Err(PairingError::EmptyField("fp")),
                    "tk" => return Err(PairingError::EmptyField("tk")),
                    _ => continue,
                }
            }
            match key {
                "ep" => endpoint = Some(value),
                "turn" => turn = Some(value),
                "fp" => fingerprint = Some(value),
                "tk" => token = Some(value),
                _ => {}
            }
        }

        let endpoint = endpoint.ok_or(PairingError::MissingField("ep"))?;
        let turn = turn.ok_or(PairingError::MissingField("turn"))?;
        let fingerprint = fingerprint.ok_or(PairingError::MissingField("fp"))?;
        let token = token.ok_or(PairingError::MissingField("tk"))?;

        if token.len() < 16 {
            return Err(PairingError::BadToken);
        }

        Ok(PairingUri {
            endpoint,
            turn,
            fingerprint,
            token,
        })
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'[' | b']' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingUri {
        PairingUri {
            endpoint: "192.168.1.10:8443".into(),
            turn: "192.168.1.10:3478".into(),
            fingerprint: "a".repeat(64),
            token: "0123456789abcdef0123".into(),
        }
    }

    #[test]
    fn roundtrip() {
        let uri = sample();
        let text = uri.to_uri();
        assert_eq!(text.parse::<PairingUri>().unwrap(), uri);
        assert!(uri.fingerprint_is_well_formed());
    }

    #[test]
    fn rejects_foreign_scheme() {
        assert_eq!(
            "https://example.com".parse::<PairingUri>(),
            Err(PairingError::WrongScheme)
        );
    }

    #[test]
    fn reports_missing_field() {
        let err = "waystone://pair?ep=a:8443&turn=a:3478&fp=00"
            .parse::<PairingUri>()
            .unwrap_err();
        assert_eq!(err, PairingError::MissingField("tk"));
    }

    #[test]
    fn ipv6_endpoint_roundtrip() {
        let mut uri = sample();
        uri.endpoint = "[fd00::1]:8443".into();
        let text = uri.to_uri();
        assert!(text.contains("ep=[fd00::1]:8443"));
        assert_eq!(text.parse::<PairingUri>().unwrap(), uri);
    }
}
