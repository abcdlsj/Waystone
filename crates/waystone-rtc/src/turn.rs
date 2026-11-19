//! Sans-IO TURN client (RFC 5766) for a single UDP allocation.
//!
//! The client never touches a socket: it emits datagrams to send and consumes
//! datagrams received from the TURN server. That keeps the WebRTC driver free to
//! own the only socket, which is also what makes the ICE connectivity checks and
//! the TURN transactions distinguishable (they share the same UDP port).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::stun::{self, attr, class, method, Builder, Message};

/// Lifetime requested for the allocation, in seconds.
pub const ALLOCATION_LIFETIME: u32 = 600;

/// Refresh once this fraction of the lifetime has elapsed.
const REFRESH_AFTER: f64 = 0.66;

/// Channel number used for the single peer of a session.
///
/// TURN reserves `0x4000..=0x7FFF` for channel numbers; one allocation only ever
/// talks to one peer, so a constant value is enough.
pub const CHANNEL_NUMBER: u16 = 0x4000;

/// Progress reported by [`TurnAllocation::handle_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Nothing changed; the message belonged to another transaction or peer.
    Idle,
    /// The server assigned us a relay address.
    Allocated(SocketAddr),
    /// The peer channel is bound and data can flow.
    Bound,
    /// The allocation was refreshed.
    Refreshed,
    /// The server refused a request.
    Failed { code: u16, reason: String },
}

/// A single TURN allocation, driven purely by datagrams.
pub struct TurnAllocation {
    server: SocketAddr,
    username: String,
    password: String,
    realm: String,
    nonce: String,
    key: Vec<u8>,
    /// Transaction id of the request we are waiting for.
    pending: Option<[u8; 12]>,
    /// Peer we want a channel to; the bind request is sent once the allocation
    /// is ready.
    wanted_peer: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    relayed: Option<SocketAddr>,
    allocated_at: Option<Instant>,
    refreshed_at: Option<Instant>,
    outgoing: Vec<Vec<u8>>,
    failed: Option<(u16, String)>,
}

impl TurnAllocation {
    /// Starts the allocation handshake.
    ///
    /// The first request is sent without credentials so the server answers `401`
    /// with the realm and nonce that long term credentials are derived from.
    pub fn start(server: SocketAddr, username: impl Into<String>, password: impl Into<String>) -> Self {
        let mut allocation = TurnAllocation {
            server,
            username: username.into(),
            password: password.into(),
            realm: String::new(),
            nonce: String::new(),
            key: Vec::new(),
            pending: None,
            wanted_peer: None,
            peer: None,
            relayed: None,
            allocated_at: None,
            refreshed_at: None,
            outgoing: Vec::new(),
            failed: None,
        };
        allocation.send_request(method::ALLOCATE, class::REQUEST, false);
        allocation
    }

    /// TURN server address.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Relay address assigned by the server, once known.
    pub fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// Bound peer, once the channel is usable.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// True when the allocation can carry data in both directions.
    pub fn is_ready(&self) -> bool {
        self.relayed.is_some()
    }

    /// Takes the datagrams queued by this allocation.
    pub fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outgoing)
    }

    /// Requests a channel to `peer`.
    ///
    /// Safe to call before the allocation is ready; the request is sent as soon as
    /// the nonce is known.
    pub fn bind_channel(&mut self, peer: SocketAddr) {
        if self.peer == Some(peer) {
            return;
        }
        self.wanted_peer = Some(peer);
        if self.relayed.is_some() && !self.nonce.is_empty() && self.failed.is_none() {
            self.send_request(method::CHANNEL_BIND, class::REQUEST, true);
        }
    }

    /// Consumes a datagram received from the TURN server.
    ///
    /// Returns [`Progress::Idle`] for anything that is not a response to our
    /// pending transaction, including ICE traffic that shares the same socket.
    pub fn handle_message(&mut self, bytes: &[u8]) -> Progress {
        if !Message::is_stun(bytes) {
            return Progress::Idle;
        }
        let Ok(message) = Message::parse(bytes) else {
            return Progress::Idle;
        };
        if self.pending != Some(message.transaction_id) {
            return Progress::Idle;
        }
        self.pending = None;

        if let Some((code, reason)) = &message.error_code {
            if *code == 401 && self.realm.is_empty() {
                // Long term credential challenge: retry authenticated.
                self.realm = message.realm.clone().unwrap_or_default();
                self.nonce = message.nonce.clone().unwrap_or_default();
                self.key = stun::long_term_key(&self.username, &self.realm, &self.password);
                self.send_request(method::ALLOCATE, class::REQUEST, true);
                return Progress::Idle;
            }
            self.failed = Some((*code, reason.clone()));
            return Progress::Failed {
                code: *code,
                reason: reason.clone(),
            };
        }

        // Learn the nonce from successful responses so later refreshes work.
        if let Some(nonce) = message.nonce.clone() {
            self.nonce = nonce;
        }

        if let Some(relayed) = message.xor_relayed_address {
            self.relayed = Some(relayed);
            self.allocated_at = Some(Instant::now());
            if let Some(peer) = self.wanted_peer {
                self.send_request(method::CHANNEL_BIND, class::REQUEST, true);
                let _ = peer;
            }
            return Progress::Allocated(relayed);
        }

        // A successful CHANNEL-BIND has no attributes we need; a REFRESH carries a
        // lifetime. Distinguishing them by the attributes is enough here.
        if message.lifetime.is_some() {
            self.refreshed_at = Some(Instant::now());
            return Progress::Refreshed;
        }

        if let Some(peer) = self.wanted_peer {
            self.peer = Some(peer);
            return Progress::Bound;
        }

        Progress::Idle
    }

    /// True when the allocation should be refreshed now.
    pub fn needs_refresh(&self, now: Instant) -> bool {
        match self.allocated_at {
            Some(allocated_at) => {
                let since = self.refreshed_at.unwrap_or(allocated_at);
                now.duration_since(since).as_secs_f64()
                    >= ALLOCATION_LIFETIME as f64 * REFRESH_AFTER
            }
            None => false,
        }
    }

    /// Queues a refresh request.
    pub fn refresh(&mut self) {
        if self.failed.is_none() {
            self.send_request(method::REFRESH, class::REQUEST, true);
        }
    }

    /// Frames `payload` as ChannelData towards the bound peer.
    ///
    /// Returns `None` when the channel is not usable yet, in which case the
    /// payload must be dropped instead of being sent to the wrong place.
    pub fn frame_channel_data(&self, peer: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
        if self.peer != Some(peer) || payload.len() > u16::MAX as usize {
            return None;
        }
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&CHANNEL_NUMBER.to_be_bytes());
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        Some(frame)
    }

    /// The failure reported by the server, if the allocation is dead.
    pub fn failure(&self) -> Option<(u16, String)> {
        self.failed.clone()
    }

    fn send_request(&mut self, method: u16, class: u16, authenticated: bool) {
        let builder = if authenticated {
            Builder::new(method, class)
                .username(&self.username)
                .realm(&self.realm)
                .nonce(&self.nonce)
        } else {
            Builder::new(method, class)
        };

        let builder = match method {
            method::ALLOCATE => builder.requested_transport_udp().lifetime(ALLOCATION_LIFETIME),
            method::REFRESH => builder.lifetime(ALLOCATION_LIFETIME),
            method::CHANNEL_BIND => {
                let peer = match self.wanted_peer {
                    Some(peer) => peer,
                    None => return,
                };
                builder.channel_number(CHANNEL_NUMBER).xor_peer_address(peer)
            }
            _ => builder,
        };

        self.pending = Some(builder.id());
        let transaction_id = self.pending.unwrap();
        let builder = builder.transaction_id(transaction_id);
        let key = if authenticated { Some(self.key.as_slice()) } else { None };
        self.outgoing.push(builder.build(key, true));
    }
}

/// Splits a ChannelData frame into its channel number and payload.
///
/// Returns `None` for STUN messages and malformed frames.
pub fn parse_channel_data(datagram: &[u8]) -> Option<(u16, &[u8])> {
    if datagram.len() < 4 {
        return None;
    }
    let channel = u16::from_be_bytes([datagram[0], datagram[1]]);
    if !(0x4000..=0x7FFF).contains(&channel) {
        return None;
    }
    let len = u16::from_be_bytes([datagram[2], datagram[3]]) as usize;
    if datagram.len() < 4 + len {
        return None;
    }
    Some((channel, &datagram[4..4 + len]))
}

/// Maps channel numbers back to peers.
#[derive(Debug, Default)]
pub struct ChannelMap {
    peers: HashMap<u16, SocketAddr>,
}

impl ChannelMap {
    pub fn insert(&mut self, channel: u16, peer: SocketAddr) {
        self.peers.insert(channel, peer);
    }

    pub fn peer(&self, channel: u16) -> Option<SocketAddr> {
        self.peers.get(&channel).copied()
    }
}

/// Attribute id exposed for logging.
pub const ATTR_RELAYED: u16 = attr::XOR_RELAYED_ADDRESS;

/// Default refresh interval, exposed so the driver can pick a timer directly.
pub fn refresh_interval() -> Duration {
    Duration::from_secs_f64(ALLOCATION_LIFETIME as f64 * REFRESH_AFTER)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn allocation_with_nonce() -> TurnAllocation {
        let mut allocation = TurnAllocation::start(
            "203.0.113.1:3478".parse().unwrap(),
            "user",
            "pass",
        );
        let request = allocation.take_outgoing();
        assert_eq!(request.len(), 1);

        let probe = Message::parse(&request[0]).unwrap();
        assert_eq!(probe.method, method::ALLOCATE);
        assert!(probe.integrity.is_none());

        // Simulate the 401 challenge carrying realm and nonce.
        let challenge = Builder::new(method::ALLOCATE, class::ERROR)
            .transaction_id(probe.transaction_id)
            .error_code(401, "Unauthorized")
            .realm("waystone")
            .nonce("nonce-1")
            .build(None, true);
        assert_eq!(allocation.handle_message(&challenge), Progress::Idle);

        allocation
    }

    #[test]
    fn challenge_then_allocation() {
        let mut allocation = allocation_with_nonce();
        assert_eq!(allocation.realm, "waystone");
        assert_eq!(allocation.nonce, "nonce-1");

        let retry = allocation.take_outgoing();
        assert_eq!(retry.len(), 1);
        let request = Message::parse(&retry[0]).unwrap();
        assert_eq!(request.method, method::ALLOCATE);
        assert_eq!(request.username.as_deref(), Some("user"));
        assert_eq!(request.lifetime, Some(ALLOCATION_LIFETIME));
        assert!(request.integrity.is_some());

        let relayed: SocketAddr = "198.51.100.7:50000".parse().unwrap();
        let response = Builder::new(method::ALLOCATE, class::SUCCESS)
            .transaction_id(request.transaction_id)
            .xor_relayed_address(relayed)
            .build(None, true);

        assert_eq!(
            allocation.handle_message(&response),
            Progress::Allocated(relayed)
        );
        assert_eq!(allocation.relayed(), Some(relayed));
        assert!(allocation.is_ready());
    }

    #[test]
    fn channel_bind_and_framing() {
        let mut allocation = allocation_with_nonce();
        let relayed: SocketAddr = "198.51.100.7:50000".parse().unwrap();
        let retry = allocation.take_outgoing();
        let request = Message::parse(&retry[0]).unwrap();
        let response = Builder::new(method::ALLOCATE, class::SUCCESS)
            .transaction_id(request.transaction_id)
            .xor_relayed_address(relayed)
            .build(None, true);
        allocation.handle_message(&response);

        let peer: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        allocation.bind_channel(peer);
        let bind = allocation.take_outgoing();
        assert_eq!(bind.len(), 1);
        let bind_request = Message::parse(&bind[0]).unwrap();
        assert_eq!(bind_request.channel_number, Some(CHANNEL_NUMBER));
        assert_eq!(bind_request.xor_peer_address, Some(peer));

        // Framing is refused until the server confirms the binding.
        assert!(allocation.frame_channel_data(peer, b"hello").is_none());

        let bound = Builder::new(method::CHANNEL_BIND, class::SUCCESS)
            .transaction_id(bind_request.transaction_id)
            .build(None, true);
        assert_eq!(allocation.handle_message(&bound), Progress::Bound);

        let frame = allocation.frame_channel_data(peer, b"hello").unwrap();
        assert_eq!(&frame[..2], &CHANNEL_NUMBER.to_be_bytes());
        let (channel, payload) = parse_channel_data(&frame).unwrap();
        assert_eq!(channel, CHANNEL_NUMBER);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn ignores_foreign_datagrams() {
        let mut allocation = allocation_with_nonce();
        assert_eq!(allocation.handle_message(&[0x80, 0x60, 0, 1]), Progress::Idle);
        let foreign = Builder::new(method::ALLOCATE, class::SUCCESS).build(None, true);
        assert_eq!(allocation.handle_message(&foreign), Progress::Idle);
    }

    #[test]
    fn reports_server_failure() {
        let mut allocation = allocation_with_nonce();
        let retry = allocation.take_outgoing();
        let request = Message::parse(&retry[0]).unwrap();
        let failure = Builder::new(method::ALLOCATE, class::ERROR)
            .transaction_id(request.transaction_id)
            .error_code(486, "Allocation Quota Reached")
            .build(None, true);
        assert_eq!(
            allocation.handle_message(&failure),
            Progress::Failed {
                code: 486,
                reason: "Allocation Quota Reached".into()
            }
        );
        assert_eq!(allocation.failure().unwrap().0, 486);
    }

    #[test]
    fn refresh_schedule() {
        let mut allocation = allocation_with_nonce();
        let retry = allocation.take_outgoing();
        let request = Message::parse(&retry[0]).unwrap();
        let response = Builder::new(method::ALLOCATE, class::SUCCESS)
            .transaction_id(request.transaction_id)
            .xor_relayed_address("198.51.100.7:50000".parse().unwrap())
            .build(None, true);
        allocation.handle_message(&response);

        let now = Instant::now();
        assert!(!allocation.needs_refresh(now));
        assert!(allocation.needs_refresh(now + refresh_interval() + Duration::from_secs(1)));
    }
}
