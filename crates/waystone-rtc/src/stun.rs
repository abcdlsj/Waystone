//! Minimal STUN (RFC 5389) message codec.
//!
//! Only the subset needed by Waystone is implemented: binding requests for server
//! reflexive discovery and the long term credential attributes used by the TURN
//! client. Keeping this in-crate means the mobile builds do not pull in a TURN
//! stack that only works on desktop platforms.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// `MAGIC-COOKIE` value mandated by RFC 5389.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;

/// Message classes, stored in the two bits the wire format reserves for them.
pub mod class {
    pub const REQUEST: u16 = 0b00;
    pub const INDICATION: u16 = 0b01;
    pub const SUCCESS: u16 = 0b10;
    pub const ERROR: u16 = 0b11;
}

pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

pub mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// Value of `REQUESTED-TRANSPORT` for UDP.
pub const TRANSPORT_UDP: u8 = 17;

const HEADER_LEN: usize = 20;

/// Errors produced while decoding a STUN message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StunError {
    #[error("datagram is shorter than the STUN header")]
    Short,
    #[error("bad magic cookie {0:#010x}")]
    BadCookie(u32),
    #[error("attribute {kind:#06x} overruns the message")]
    BadAttribute { kind: u16 },
    #[error("address family {0} is not supported")]
    BadFamily(u8),
    #[error("message integrity mismatch")]
    Integrity,
}

/// A decoded STUN message. Attributes of interest are kept, the rest is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub method: u16,
    pub class: u16,
    pub transaction_id: [u8; 12],
    pub error_code: Option<(u16, String)>,
    pub mapped_address: Option<SocketAddr>,
    pub xor_mapped_address: Option<SocketAddr>,
    pub xor_relayed_address: Option<SocketAddr>,
    pub xor_peer_address: Option<SocketAddr>,
    pub username: Option<String>,
    pub realm: Option<String>,
    pub nonce: Option<String>,
    pub lifetime: Option<u32>,
    pub channel_number: Option<u16>,
    pub data: Option<Vec<u8>>,
    /// Byte range of the `MESSAGE-INTEGRITY` value inside the raw datagram.
    pub integrity: Option<Vec<u8>>,
    /// Raw prefix covered by the integrity computation.
    pub integrity_prefix: Vec<u8>,
}

impl Message {
    /// True when the datagram looks like a STUN message rather than RTP/RTCP.
    pub fn is_stun(bytes: &[u8]) -> bool {
        bytes.len() >= HEADER_LEN && bytes[0] & 0xC0 == 0 && read_u32(&bytes[4..8]) == MAGIC_COOKIE
    }

    /// Parses a datagram.
    pub fn parse(bytes: &[u8]) -> Result<Self, StunError> {
        if bytes.len() < HEADER_LEN {
            return Err(StunError::Short);
        }
        let cookie = read_u32(&bytes[4..8]);
        if cookie != MAGIC_COOKIE {
            return Err(StunError::BadCookie(cookie));
        }
        let kind = read_u16(&bytes[0..2]);
        let method = (kind & 0x000F) | ((kind & 0x00E0) >> 1) | ((kind & 0x3E00) >> 2);
        let class = ((kind >> 4) & 0x1) | ((kind >> 7) & 0x2);
        let length = read_u16(&bytes[2..4]) as usize;
        let end = (HEADER_LEN + length).min(bytes.len());

        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&bytes[8..20]);

        let mut message = Message {
            method,
            class,
            transaction_id,
            ..Default::default()
        };

        let mut offset = HEADER_LEN;
        while offset + 4 <= end {
            let kind = read_u16(&bytes[offset..offset + 2]);
            let len = read_u16(&bytes[offset + 2..offset + 4]) as usize;
            let value_start = offset + 4;
            let value_end = value_start + len;
            if value_end > end {
                return Err(StunError::BadAttribute { kind });
            }
            let value = &bytes[value_start..value_end];

            match kind {
                attr::MAPPED_ADDRESS => message.mapped_address = decode_address(value, false).ok(),
                attr::XOR_MAPPED_ADDRESS => {
                    message.xor_mapped_address = decode_address(value, true).ok()
                }
                attr::XOR_RELAYED_ADDRESS => {
                    message.xor_relayed_address = decode_address(value, true).ok()
                }
                attr::XOR_PEER_ADDRESS => {
                    message.xor_peer_address = decode_address(value, true).ok()
                }
                attr::USERNAME => message.username = Some(lossy(value)),
                attr::REALM => message.realm = Some(lossy(value)),
                attr::NONCE => message.nonce = Some(lossy(value)),
                attr::LIFETIME if len >= 4 => message.lifetime = Some(read_u32(value)),
                attr::CHANNEL_NUMBER if len >= 2 => message.channel_number = Some(read_u16(value)),
                attr::DATA => message.data = Some(value.to_vec()),
                attr::ERROR_CODE if len >= 4 => {
                    let class = value[2] & 0x07;
                    let number = value[3];
                    let code = (class as u16) * 100 + number as u16;
                    message.error_code = Some((code, lossy(&value[4..])));
                }
                attr::MESSAGE_INTEGRITY => {
                    message.integrity = Some(value.to_vec());
                    // RFC 5389: the HMAC covers the message up to, but excluding,
                    // the MESSAGE-INTEGRITY attribute itself, with the length field
                    // already adjusted to include it.
                    message.integrity_prefix = bytes[..offset].to_vec();
                }
                _ => {}
            }

            // Attributes are padded to a four byte boundary.
            offset = value_start + len.div_ceil(4) * 4;
        }

        Ok(message)
    }

    /// Verifies `MESSAGE-INTEGRITY` against a long term credential key.
    pub fn verify_integrity(&self, key: &[u8]) -> Result<(), StunError> {
        let integrity = self.integrity.as_ref().ok_or(StunError::Integrity)?;
        let expected = hmac_sha1(key, &self.integrity_prefix);
        if expected.as_slice() == integrity.as_slice() {
            Ok(())
        } else {
            Err(StunError::Integrity)
        }
    }
}

/// Builds a STUN request or indication.
pub struct Builder {
    method: u16,
    class: u16,
    transaction_id: [u8; 12],
    attributes: Vec<u8>,
}

impl Builder {
    /// Starts a message with a random transaction id.
    pub fn new(method: u16, class: u16) -> Self {
        let mut transaction_id = [0u8; 12];
        rand::Rng::fill(&mut rand::rng(), &mut transaction_id);
        Builder {
            method,
            class,
            transaction_id,
            attributes: Vec::with_capacity(64),
        }
    }

    /// Overrides the random transaction id, used by tests.
    pub fn transaction_id(mut self, id: [u8; 12]) -> Self {
        self.transaction_id = id;
        self
    }

    /// The transaction id chosen for this message.
    pub fn id(&self) -> [u8; 12] {
        self.transaction_id
    }

    /// Appends a `USERNAME` attribute.
    pub fn username(mut self, value: &str) -> Self {
        self.push(attr::USERNAME, value.as_bytes());
        self
    }

    /// Appends a `REALM` attribute.
    pub fn realm(mut self, value: &str) -> Self {
        self.push(attr::REALM, value.as_bytes());
        self
    }

    /// Appends a `NONCE` attribute.
    pub fn nonce(mut self, value: &str) -> Self {
        self.push(attr::NONCE, value.as_bytes());
        self
    }

    /// Appends a `LIFETIME` attribute.
    pub fn lifetime(mut self, seconds: u32) -> Self {
        self.push(attr::LIFETIME, &seconds.to_be_bytes());
        self
    }

    /// Appends a `REQUESTED-TRANSPORT` attribute set to UDP.
    pub fn requested_transport_udp(mut self) -> Self {
        let mut value = [0u8; 4];
        value[0] = TRANSPORT_UDP;
        self.push(attr::REQUESTED_TRANSPORT, &value);
        self
    }

    /// Appends a `CHANNEL-NUMBER` attribute.
    pub fn channel_number(mut self, number: u16) -> Self {
        let mut value = [0u8; 4];
        value[..2].copy_from_slice(&number.to_be_bytes());
        self.push(attr::CHANNEL_NUMBER, &value);
        self
    }

    /// Appends a `XOR-PEER-ADDRESS` attribute.
    pub fn xor_peer_address(mut self, address: SocketAddr) -> Self {
        let value = encode_address(address, true);
        self.push(attr::XOR_PEER_ADDRESS, &value);
        self
    }

    /// Appends a `XOR-RELAYED-ADDRESS` attribute.
    pub fn xor_relayed_address(mut self, address: SocketAddr) -> Self {
        let value = encode_address(address, true);
        self.push(attr::XOR_RELAYED_ADDRESS, &value);
        self
    }

    /// Appends a `XOR-MAPPED-ADDRESS` attribute.
    pub fn xor_mapped_address(mut self, address: SocketAddr) -> Self {
        let value = encode_address(address, true);
        self.push(attr::XOR_MAPPED_ADDRESS, &value);
        self
    }

    /// Appends an `ERROR-CODE` attribute.
    pub fn error_code(mut self, code: u16, reason: &str) -> Self {
        let class = (code / 100) as u8;
        let number = (code % 100) as u8;
        let mut value = vec![0u8, 0u8, class & 0x07, number];
        value.extend_from_slice(reason.as_bytes());
        self.push(attr::ERROR_CODE, &value);
        self
    }

    /// Serializes the message, optionally appending `MESSAGE-INTEGRITY` and
    /// `FINGERPRINT`.
    ///
    /// `key` is the long term credential key (MD5 of `user:realm:password`).
    pub fn build(self, key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        const MI_ATTR_LEN: usize = 24;
        const FP_ATTR_LEN: usize = 8;

        let mut trailer = 0usize;
        if key.is_some() {
            trailer += MI_ATTR_LEN;
        }
        if fingerprint {
            trailer += FP_ATTR_LEN;
        }

        let mut header = Vec::with_capacity(HEADER_LEN + self.attributes.len() + trailer);
        header.extend_from_slice(&encode_type(self.method, self.class).to_be_bytes());
        // The length field must already include every attribute that follows,
        // because MESSAGE-INTEGRITY covers the header as it will be sent.
        header.extend_from_slice(&((self.attributes.len() + trailer) as u16).to_be_bytes());
        header.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        header.extend_from_slice(&self.transaction_id);
        header.extend_from_slice(&self.attributes);

        if let Some(key) = key {
            let digest = hmac_sha1(key, &header);
            header.extend_from_slice(&attr::MESSAGE_INTEGRITY.to_be_bytes());
            header.extend_from_slice(&20u16.to_be_bytes());
            header.extend_from_slice(&digest);
        }

        if fingerprint {
            let crc = crc32fast::hash(&header) ^ 0x5354_554E;
            header.extend_from_slice(&attr::FINGERPRINT.to_be_bytes());
            header.extend_from_slice(&4u16.to_be_bytes());
            header.extend_from_slice(&crc.to_be_bytes());
        }

        header
    }

    fn push(&mut self, kind: u16, value: &[u8]) {
        self.attributes.extend_from_slice(&kind.to_be_bytes());
        self.attributes
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.attributes.extend_from_slice(value);
        while self.attributes.len() % 4 != 0 {
            self.attributes.push(0);
        }
    }
}

/// Computes the long term credential key used by TURN: `MD5(user:realm:password)`.
pub fn long_term_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(format!("{username}:{realm}:{password}").as_bytes());
    hasher.finalize().to_vec()
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn encode_type(method: u16, class: u16) -> u16 {
    let m = method;
    (m & 0x000F)
        | ((m & 0x0070) << 1)
        | ((m & 0x0F80) << 2)
        | ((class & 0x1) << 4)
        | ((class & 0x2) << 7)
}

fn decode_address(value: &[u8], xored: bool) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::Short);
    }
    let family = value[1];
    let port = read_u16(&value[2..4])
        ^ if xored {
            (MAGIC_COOKIE >> 16) as u16
        } else {
            0
        };
    let ip: IpAddr = match family {
        0x01 => {
            if value.len() < 8 {
                return Err(StunError::Short);
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&value[4..8]);
            if xored {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (i, byte) in octets.iter_mut().enumerate() {
                    *byte ^= cookie[i];
                }
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        0x02 => {
            if value.len() < 20 {
                return Err(StunError::Short);
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            if xored {
                let mut mask = [0u8; 16];
                mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                mask[4..].copy_from_slice(&[0u8; 12]);
                for (i, byte) in octets.iter_mut().enumerate() {
                    *byte ^= mask[i];
                }
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        other => return Err(StunError::BadFamily(other)),
    };
    Ok(SocketAddr::new(ip, port))
}

fn encode_address(address: SocketAddr, xored: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.push(0);
    match address.ip() {
        IpAddr::V4(ip) => {
            out.push(0x01);
            let port = address.port()
                ^ if xored {
                    (MAGIC_COOKIE >> 16) as u16
                } else {
                    0
                };
            out.extend_from_slice(&port.to_be_bytes());
            let mut octets = ip.octets();
            if xored {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (i, byte) in octets.iter_mut().enumerate() {
                    *byte ^= cookie[i];
                }
            }
            out.extend_from_slice(&octets);
        }
        IpAddr::V6(ip) => {
            out.push(0x02);
            let port = address.port()
                ^ if xored {
                    (MAGIC_COOKIE >> 16) as u16
                } else {
                    0
                };
            out.extend_from_slice(&port.to_be_bytes());
            let mut octets = ip.octets();
            if xored {
                let mut mask = [0u8; 16];
                mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                for (i, byte) in octets.iter_mut().enumerate() {
                    *byte ^= mask[i];
                }
            }
            out.extend_from_slice(&octets);
        }
    }
    out
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_request_roundtrip() {
        let bytes = Builder::new(method::BINDING, class::REQUEST)
            .transaction_id([9u8; 12])
            .build(None, true);
        assert!(Message::is_stun(&bytes));

        let message = Message::parse(&bytes).unwrap();
        assert_eq!(message.method, method::BINDING);
        assert_eq!(message.class, class::REQUEST);
        assert_eq!(message.transaction_id, [9u8; 12]);
    }

    #[test]
    fn xor_address_roundtrip_ipv4() {
        let address: SocketAddr = "203.0.113.9:49152".parse().unwrap();
        let encoded = encode_address(address, true);
        assert_eq!(decode_address(&encoded, true).unwrap(), address);
        // Big endian, the IPv4 part is XORed with the magic cookie.
        assert_eq!(&encoded[4..8], &[0xEA, 0x12, 0xD5, 0x4B]);
    }

    #[test]
    fn xor_address_roundtrip_ipv6() {
        let address: SocketAddr = "[fd00::1]:3478".parse().unwrap();
        let encoded = encode_address(address, true);
        assert_eq!(decode_address(&encoded, true).unwrap(), address);
    }

    #[test]
    fn integrity_is_verified_with_the_long_term_key() {
        let key = long_term_key("user", "waystone", "pass");
        let bytes = Builder::new(method::ALLOCATE, class::REQUEST)
            .username("user")
            .realm("waystone")
            .nonce("abc")
            .lifetime(600)
            .requested_transport_udp()
            .build(Some(&key), true);

        let message = Message::parse(&bytes).unwrap();
        assert_eq!(message.username.as_deref(), Some("user"));
        assert_eq!(message.nonce.as_deref(), Some("abc"));
        assert_eq!(message.lifetime, Some(600));
        message.verify_integrity(&key).unwrap();
        assert!(message.verify_integrity(b"wrong").is_err());
    }

    #[test]
    fn type_encoding_matches_rfc() {
        // Values taken from RFC 5389 section 6.
        assert_eq!(encode_type(method::BINDING, class::REQUEST), 0x0001);
        assert_eq!(encode_type(method::BINDING, class::SUCCESS), 0x0101);
        assert_eq!(encode_type(method::BINDING, class::ERROR), 0x0111);
        assert_eq!(encode_type(method::ALLOCATE, class::REQUEST), 0x0003);
        assert_eq!(encode_type(method::CHANNEL_BIND, class::REQUEST), 0x0009);
    }

    #[test]
    fn rejects_non_stun_datagrams() {
        assert!(!Message::is_stun(&[0x80, 0x00, 0x00, 0x00]));
        assert!(!Message::is_stun(&[0x00, 0x01]));
        let mut rtp = vec![0x80, 0x60, 0x00, 0x01, 0, 0, 0, 0];
        rtp.extend_from_slice(&[0u8; 16]);
        assert!(!Message::is_stun(&rtp));
    }
}
