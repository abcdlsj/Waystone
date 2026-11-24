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
    /// Value of the `MESSAGE-INTEGRITY` attribute, when present.
    pub integrity: Option<Vec<u8>>,
    /// Bytes the integrity HMAC covers: everything before `MESSAGE-INTEGRITY`,
    /// with the header length rewritten to end right after that attribute
    /// (RFC 5389 section 15.4), so a trailing `FINGERPRINT` is excluded.
    pub integrity_prefix: Vec<u8>,
    /// `Some(valid)` when the message carries a `FINGERPRINT` attribute.
    pub fingerprint: Option<bool>,
}

impl Message {
    /// True when the datagram looks like a STUN message rather than RTP/RTCP.
    pub fn is_stun(bytes: &[u8]) -> bool {
        bytes.len() >= HEADER_LEN && bytes[0] & 0xC0 == 0 && read_u32(&bytes[4..8]) == MAGIC_COOKIE
    }

    /// Parses a datagram.
    ///
    /// Attributes that follow `MESSAGE-INTEGRITY` are not authenticated, so only
    /// `FINGERPRINT` is honoured after it (RFC 5389 section 15.4).
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

            if kind == attr::FINGERPRINT {
                let expected = crc32fast::hash(&bytes[..offset]) ^ FINGERPRINT_XOR;
                message.fingerprint = Some(len == 4 && read_u32(value) == expected);
                // FINGERPRINT is always the last attribute.
                break;
            }

            if message.integrity.is_none() {
                message.read_attribute(kind, value, &transaction_id);
                if kind == attr::MESSAGE_INTEGRITY {
                    let mut prefix = bytes[..offset].to_vec();
                    let covered = (value_end - HEADER_LEN) as u16;
                    prefix[2..4].copy_from_slice(&covered.to_be_bytes());
                    message.integrity_prefix = prefix;
                }
            }

            // Attributes are padded to a four byte boundary.
            offset = value_start + len.div_ceil(4) * 4;
        }

        Ok(message)
    }

    fn read_attribute(&mut self, kind: u16, value: &[u8], transaction_id: &[u8; 12]) {
        let len = value.len();
        match kind {
            attr::MAPPED_ADDRESS => self.mapped_address = decode_address(value, None).ok(),
            attr::XOR_MAPPED_ADDRESS => {
                self.xor_mapped_address = decode_address(value, Some(transaction_id)).ok()
            }
            attr::XOR_RELAYED_ADDRESS => {
                self.xor_relayed_address = decode_address(value, Some(transaction_id)).ok()
            }
            attr::XOR_PEER_ADDRESS => {
                self.xor_peer_address = decode_address(value, Some(transaction_id)).ok()
            }
            attr::USERNAME => self.username = Some(lossy(value)),
            attr::REALM => self.realm = Some(lossy(value)),
            attr::NONCE => self.nonce = Some(lossy(value)),
            attr::LIFETIME if len >= 4 => self.lifetime = Some(read_u32(value)),
            attr::CHANNEL_NUMBER if len >= 2 => self.channel_number = Some(read_u16(value)),
            attr::DATA => self.data = Some(value.to_vec()),
            attr::ERROR_CODE if len >= 4 => {
                let class = value[2] & 0x07;
                let number = value[3];
                let code = (class as u16) * 100 + number as u16;
                self.error_code = Some((code, lossy(&value[4..])));
            }
            attr::MESSAGE_INTEGRITY if len == 20 => self.integrity = Some(value.to_vec()),
            _ => {}
        }
    }

    /// Verifies `MESSAGE-INTEGRITY` with `key`: the long term credential key
    /// (see [`long_term_key`]) or the raw password for short term credentials.
    pub fn verify_integrity(&self, key: &[u8]) -> Result<(), StunError> {
        let integrity = self.integrity.as_ref().ok_or(StunError::Integrity)?;
        let expected = hmac_sha1(key, &self.integrity_prefix);
        if constant_time_eq(&expected, integrity) {
            Ok(())
        } else {
            Err(StunError::Integrity)
        }
    }
}

enum Attribute {
    Raw(u16, Vec<u8>),
    /// A value whose padding uses a specific byte; RFC 5769 pads with spaces.
    #[cfg(test)]
    Padded(u16, Vec<u8>, u8),
    /// XOR-encoded addresses depend on the transaction id, which may still
    /// change, so they are encoded in [`Builder::build`].
    XorAddress(u16, SocketAddr),
}

/// Builds a STUN request or indication.
pub struct Builder {
    method: u16,
    class: u16,
    transaction_id: [u8; 12],
    attributes: Vec<Attribute>,
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
            attributes: Vec::new(),
        }
    }

    /// Overrides the random transaction id.
    pub fn transaction_id(mut self, id: [u8; 12]) -> Self {
        self.transaction_id = id;
        self
    }

    /// The transaction id chosen for this message.
    pub fn id(&self) -> [u8; 12] {
        self.transaction_id
    }

    /// Appends an arbitrary attribute; the value is padded automatically.
    pub fn attribute(mut self, kind: u16, value: &[u8]) -> Self {
        self.attributes.push(Attribute::Raw(kind, value.to_vec()));
        self
    }

    #[cfg(test)]
    fn attribute_padded(mut self, kind: u16, value: &[u8], pad: u8) -> Self {
        self.attributes
            .push(Attribute::Padded(kind, value.to_vec(), pad));
        self
    }

    /// Appends a `USERNAME` attribute.
    pub fn username(self, value: &str) -> Self {
        self.attribute(attr::USERNAME, value.as_bytes())
    }

    /// Appends a `REALM` attribute.
    pub fn realm(self, value: &str) -> Self {
        self.attribute(attr::REALM, value.as_bytes())
    }

    /// Appends a `NONCE` attribute.
    pub fn nonce(self, value: &str) -> Self {
        self.attribute(attr::NONCE, value.as_bytes())
    }

    /// Appends a `LIFETIME` attribute.
    pub fn lifetime(self, seconds: u32) -> Self {
        self.attribute(attr::LIFETIME, &seconds.to_be_bytes())
    }

    /// Appends a `REQUESTED-TRANSPORT` attribute set to UDP.
    pub fn requested_transport_udp(self) -> Self {
        self.attribute(attr::REQUESTED_TRANSPORT, &[TRANSPORT_UDP, 0, 0, 0])
    }

    /// Appends a `CHANNEL-NUMBER` attribute.
    pub fn channel_number(self, number: u16) -> Self {
        let [high, low] = number.to_be_bytes();
        self.attribute(attr::CHANNEL_NUMBER, &[high, low, 0, 0])
    }

    /// Appends a `DATA` attribute.
    pub fn data(self, payload: &[u8]) -> Self {
        self.attribute(attr::DATA, payload)
    }

    /// Appends a `XOR-PEER-ADDRESS` attribute.
    pub fn xor_peer_address(mut self, address: SocketAddr) -> Self {
        self.attributes
            .push(Attribute::XorAddress(attr::XOR_PEER_ADDRESS, address));
        self
    }

    /// Appends a `XOR-RELAYED-ADDRESS` attribute.
    pub fn xor_relayed_address(mut self, address: SocketAddr) -> Self {
        self.attributes
            .push(Attribute::XorAddress(attr::XOR_RELAYED_ADDRESS, address));
        self
    }

    /// Appends a `XOR-MAPPED-ADDRESS` attribute.
    pub fn xor_mapped_address(mut self, address: SocketAddr) -> Self {
        self.attributes
            .push(Attribute::XorAddress(attr::XOR_MAPPED_ADDRESS, address));
        self
    }

    /// Appends an `ERROR-CODE` attribute.
    pub fn error_code(self, code: u16, reason: &str) -> Self {
        let class = (code / 100) as u8;
        let number = (code % 100) as u8;
        let mut value = vec![0u8, 0u8, class & 0x07, number];
        value.extend_from_slice(reason.as_bytes());
        self.attribute(attr::ERROR_CODE, &value)
    }

    /// Serializes the message, optionally appending `MESSAGE-INTEGRITY` and
    /// `FINGERPRINT`.
    ///
    /// `key` is the long term credential key (MD5 of `user:realm:password`) or
    /// the short term password.
    pub fn build(self, key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        const MI_ATTR_LEN: usize = 24;
        const FP_ATTR_LEN: usize = 8;

        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&encode_type(self.method, self.class).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id);
        for attribute in &self.attributes {
            match attribute {
                Attribute::Raw(kind, value) => push_attribute(&mut out, *kind, value),
                #[cfg(test)]
                Attribute::Padded(kind, value, pad) => {
                    push_attribute(&mut out, *kind, value);
                    let padding = value.len().div_ceil(4) * 4 - value.len();
                    let end = out.len();
                    out[end - padding..].fill(*pad);
                }
                Attribute::XorAddress(kind, address) => {
                    let value = encode_address(*address, Some(&self.transaction_id));
                    push_attribute(&mut out, *kind, &value);
                }
            }
        }

        if let Some(key) = key {
            // The HMAC covers the header with a length that ends right after
            // MESSAGE-INTEGRITY; a FINGERPRINT appended later is not included.
            let length = out.len() - HEADER_LEN + MI_ATTR_LEN;
            set_length(&mut out, length);
            let digest = hmac_sha1(key, &out);
            push_attribute(&mut out, attr::MESSAGE_INTEGRITY, &digest);
        }

        if fingerprint {
            let length = out.len() - HEADER_LEN + FP_ATTR_LEN;
            set_length(&mut out, length);
            let crc = crc32fast::hash(&out) ^ FINGERPRINT_XOR;
            push_attribute(&mut out, attr::FINGERPRINT, &crc.to_be_bytes());
        }

        let length = out.len() - HEADER_LEN;
        set_length(&mut out, length);
        out
    }
}

/// XOR mask applied to the CRC-32 in `FINGERPRINT` ("STUN").
const FINGERPRINT_XOR: u32 = 0x5354_554E;

fn push_attribute(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn set_length(out: &mut [u8], length: usize) {
    out[2..4].copy_from_slice(&(length as u16).to_be_bytes());
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn encode_type(method: u16, class: u16) -> u16 {
    let m = method;
    (m & 0x000F)
        | ((m & 0x0070) << 1)
        | ((m & 0x0F80) << 2)
        | ((class & 0x1) << 4)
        | ((class & 0x2) << 7)
}

/// The XOR mask for addresses: the magic cookie followed by the transaction id.
/// IPv4 only uses the first four bytes (RFC 5389 section 15.2).
fn xor_mask(transaction_id: &[u8; 12]) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(transaction_id);
    mask
}

/// Decodes a (XOR-)MAPPED-ADDRESS style value. `transaction_id` is `Some` for
/// the XOR variants.
fn decode_address(
    value: &[u8],
    transaction_id: Option<&[u8; 12]>,
) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::Short);
    }
    let family = value[1];
    let mask = transaction_id.map(xor_mask);
    let mut port = read_u16(&value[2..4]);
    if mask.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip: IpAddr = match family {
        0x01 => {
            if value.len() < 8 {
                return Err(StunError::Short);
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&value[4..8]);
            if let Some(mask) = mask {
                octets.iter_mut().zip(mask).for_each(|(byte, m)| *byte ^= m);
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        0x02 => {
            if value.len() < 20 {
                return Err(StunError::Short);
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            if let Some(mask) = mask {
                octets.iter_mut().zip(mask).for_each(|(byte, m)| *byte ^= m);
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        other => return Err(StunError::BadFamily(other)),
    };
    Ok(SocketAddr::new(ip, port))
}

fn encode_address(address: SocketAddr, transaction_id: Option<&[u8; 12]>) -> Vec<u8> {
    let mask = transaction_id.map(xor_mask);
    let mut port = address.port();
    if mask.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let (family, mut octets) = match address.ip() {
        IpAddr::V4(ip) => (0x01, ip.octets().to_vec()),
        IpAddr::V6(ip) => (0x02, ip.octets().to_vec()),
    };
    if let Some(mask) = mask {
        octets.iter_mut().zip(mask).for_each(|(byte, m)| *byte ^= m);
    }
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(&[0, family]);
    out.extend_from_slice(&port.to_be_bytes());
    out.extend_from_slice(&octets);
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

    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(|byte| byte.is_ascii_hexdigit())
            .collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// RFC 5769 section 2.1: request with short term credentials, carrying
    /// MESSAGE-INTEGRITY followed by FINGERPRINT.
    const RFC5769_REQUEST: &str = "
        00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
        80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
        00 24 00 04 6e 00 01 ff
        80 29 00 08 93 2f f9 b1 51 26 3b 36
        00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
        00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
        80 28 00 04 e5 7a 3b cf";

    /// RFC 5769 section 2.2: IPv4 success response.
    const RFC5769_RESPONSE_V4: &str = "
        01 01 00 3c 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
        80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
        00 20 00 08 00 01 a1 47 e1 12 a6 43
        00 08 00 14 2b 91 f5 99 fd 9e 90 c3 8c 74 89 f9 2a f9 ba 53 f0 6b e7 d7
        80 28 00 04 c0 7d 4c 96";

    /// RFC 5769 section 2.3: IPv6 success response.
    const RFC5769_RESPONSE_V6: &str = "
        01 01 00 48 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
        80 22 00 0b 74 65 73 74 20 76 65 63 74 6f 72 20
        00 20 00 14 00 02 a1 47 01 13 a9 fa a5 d3 f1 79 bc 25 f4 b5 be d2 b9 d9
        00 08 00 14 a3 82 95 4e 4b e6 7b f1 17 84 c9 7c 82 92 c2 75 bf e3 ed 41
        80 28 00 04 c8 fb 0b 4c";

    const RFC5769_PASSWORD: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";

    #[test]
    fn rfc5769_request_integrity_and_fingerprint() {
        let bytes = hex(RFC5769_REQUEST);
        let message = Message::parse(&bytes).unwrap();
        assert_eq!(message.method, method::BINDING);
        assert_eq!(message.class, class::REQUEST);
        assert_eq!(message.username.as_deref(), Some("evtj:h6vY"));
        assert_eq!(message.fingerprint, Some(true));
        message.verify_integrity(RFC5769_PASSWORD).unwrap();
        assert!(message.verify_integrity(b"wrong").is_err());
    }

    #[test]
    fn rfc5769_request_is_reproduced_bit_for_bit() {
        // Encoding the same attributes must yield the exact RFC bytes. The RFC
        // pads USERNAME with spaces, and the padding is covered by the HMAC.
        let expected = hex(RFC5769_REQUEST);
        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&expected[8..20]);
        let built = Builder::new(method::BINDING, class::REQUEST)
            .transaction_id(transaction_id)
            .attribute(0x8022, b"STUN test client")
            .attribute(0x0024, &[0x6e, 0x00, 0x01, 0xff])
            .attribute(0x8029, &[0x93, 0x2f, 0xf9, 0xb1, 0x51, 0x26, 0x3b, 0x36])
            .attribute_padded(attr::USERNAME, b"evtj:h6vY", b' ')
            .build(Some(RFC5769_PASSWORD), true);
        assert_eq!(built, expected);
    }

    #[test]
    fn rfc5769_ipv4_response() {
        let message = Message::parse(&hex(RFC5769_RESPONSE_V4)).unwrap();
        assert_eq!(message.class, class::SUCCESS);
        assert_eq!(
            message.xor_mapped_address,
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert_eq!(message.fingerprint, Some(true));
        message.verify_integrity(RFC5769_PASSWORD).unwrap();
    }

    #[test]
    fn rfc5769_ipv6_response() {
        let message = Message::parse(&hex(RFC5769_RESPONSE_V6)).unwrap();
        assert_eq!(
            message.xor_mapped_address,
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
        assert_eq!(message.fingerprint, Some(true));
        message.verify_integrity(RFC5769_PASSWORD).unwrap();
    }

    #[test]
    fn corrupted_fingerprint_is_reported() {
        let mut bytes = hex(RFC5769_REQUEST);
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(Message::parse(&bytes).unwrap().fingerprint, Some(false));
    }

    #[test]
    fn attributes_after_integrity_are_ignored() {
        let key = long_term_key("user", "waystone", "pass");
        let mut bytes = Builder::new(method::ALLOCATE, class::SUCCESS)
            .lifetime(600)
            .build(Some(&key), false);
        // Append an unauthenticated LIFETIME after MESSAGE-INTEGRITY.
        push_attribute(&mut bytes, attr::LIFETIME, &1u32.to_be_bytes());
        let length = bytes.len() - HEADER_LEN;
        set_length(&mut bytes, length);
        let message = Message::parse(&bytes).unwrap();
        assert_eq!(message.lifetime, Some(600));
        message.verify_integrity(&key).unwrap();
    }

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
        assert_eq!(message.fingerprint, Some(true));
    }

    #[test]
    fn xor_address_roundtrip_ipv4() {
        let address: SocketAddr = "203.0.113.9:49152".parse().unwrap();
        let id = [7u8; 12];
        let encoded = encode_address(address, Some(&id));
        assert_eq!(decode_address(&encoded, Some(&id)).unwrap(), address);
        // Big endian, the IPv4 part is XORed with the magic cookie only.
        assert_eq!(&encoded[4..8], &[0xEA, 0x12, 0xD5, 0x4B]);
    }

    #[test]
    fn xor_address_roundtrip_ipv6() {
        let address: SocketAddr = "[fd00::1]:3478".parse().unwrap();
        let id = [0xA5u8; 12];
        let encoded = encode_address(address, Some(&id));
        assert_eq!(decode_address(&encoded, Some(&id)).unwrap(), address);
        // The low 96 bits are XORed with the transaction id.
        assert_eq!(encoded[19], 0x01 ^ 0xA5);
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
        assert_eq!(message.fingerprint, Some(true));
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
