//! Sans-IO TURN client (RFC 5766 / RFC 8656) for one UDP allocation.
//!
//! The client never touches a socket: the driver feeds it the datagrams that
//! came from the TURN server and the current time, and drains the datagrams it
//! wants to send. That keeps the WebRTC driver the single owner of the only UDP
//! socket, which ICE and TURN share.
//!
//! One allocation serves any number of peers. Each peer IP gets a permission
//! (refreshed every four minutes, they expire after five) and each peer address
//! gets its own channel (refreshed every nine minutes, they expire after ten).
//! Every request is tracked in a transaction table and retransmitted with the
//! RFC 5389 back-off until it is answered or gives up.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun::{self, class, method, Builder, Message};

/// Lifetime requested for the allocation, in seconds.
pub const ALLOCATION_LIFETIME: u32 = 600;

/// Permissions last five minutes; refresh well before that.
const PERMISSION_REFRESH: Duration = Duration::from_secs(4 * 60);
/// Channel bindings last ten minutes; refresh well before that.
const CHANNEL_REFRESH: Duration = Duration::from_secs(9 * 60);
/// Back-off before a failed permission, channel or refresh request is retried.
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(10);

/// RFC 5389 section 7.2.1 defaults: RTO 500 ms, Rc = 7, Rm = 16.
const INITIAL_RTO: Duration = Duration::from_millis(500);
const MAX_TRANSMISSIONS: u32 = 7;
const FINAL_WAIT_FACTOR: u32 = 16;

/// How many 438 Stale Nonce answers in a row a request may get before it fails.
const MAX_STALE_NONCE: u32 = 3;

/// Channel numbers RFC 8656 allows clients to use.
const FIRST_CHANNEL: u16 = 0x4000;
const LAST_CHANNEL: u16 = 0x4FFF;

/// Something the driver should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server assigned us a relay address.
    Allocated(SocketAddr),
    /// The allocation is gone and will not come back.
    Failed { code: u16, reason: String },
}

/// Result of [`TurnClient::handle_datagram`].
#[derive(Debug, PartialEq, Eq)]
pub enum Received<'a> {
    /// A datagram a peer sent to our relay address.
    Peer {
        peer: SocketAddr,
        payload: Cow<'a, [u8]>,
    },
    /// A TURN control message; nothing to forward.
    Consumed,
    /// Not TURN traffic (for example a STUN check from a peer that happens to
    /// share the server address); the caller should handle it normally.
    NotTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Allocate,
    Refresh { lifetime: u32 },
    CreatePermission(IpAddr),
    ChannelBind { peer: SocketAddr, number: u16 },
}

struct Transaction {
    request: Request,
    bytes: Vec<u8>,
    authenticated: bool,
    /// Next retransmission, or the give-up time after the last one.
    deadline: Instant,
    rto: Duration,
    transmissions: u32,
    stale_nonces: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the Allocate response.
    Allocating,
    Ready {
        relayed: SocketAddr,
        refresh_at: Instant,
        expires_at: Instant,
    },
    Failed,
}

struct Permission {
    installed: bool,
    pending: bool,
    refresh_at: Instant,
}

struct Channel {
    number: u16,
    bound: bool,
    pending: bool,
    refresh_at: Instant,
}

/// A TURN allocation, driven purely by datagrams and time.
pub struct TurnClient {
    server: SocketAddr,
    username: String,
    password: String,
    realm: Option<String>,
    nonce: Option<String>,
    key: Option<Vec<u8>>,
    state: State,
    failure: Option<(u16, String)>,
    transactions: HashMap<[u8; 12], Transaction>,
    permissions: HashMap<IpAddr, Permission>,
    channels: HashMap<SocketAddr, Channel>,
    next_channel: u16,
    outgoing: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
}

impl TurnClient {
    /// Starts the allocation handshake.
    ///
    /// The first request carries no credentials so the server answers `401` with
    /// the realm and nonce the long term key is derived from.
    pub fn new(
        server: SocketAddr,
        username: impl Into<String>,
        password: impl Into<String>,
        now: Instant,
    ) -> Self {
        let mut client = TurnClient {
            server,
            username: username.into(),
            password: password.into(),
            realm: None,
            nonce: None,
            key: None,
            state: State::Allocating,
            failure: None,
            transactions: HashMap::new(),
            permissions: HashMap::new(),
            channels: HashMap::new(),
            next_channel: FIRST_CHANNEL,
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
        };
        client.start(Request::Allocate, now, 0);
        client
    }

    /// TURN server address; every datagram from [`Self::poll_transmit`] goes here.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Relay address assigned by the server, once known.
    pub fn relayed(&self) -> Option<SocketAddr> {
        match self.state {
            State::Ready { relayed, .. } => Some(relayed),
            _ => None,
        }
    }

    /// The failure reported by the server, if the allocation is dead.
    pub fn failure(&self) -> Option<(u16, String)> {
        self.failure.clone()
    }

    /// Next datagram to send to the server.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }

    /// Next event for the driver.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// When [`Self::handle_timeout`] wants to run next.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let transactions = self.transactions.values().map(|t| t.deadline);
        let State::Ready {
            refresh_at,
            expires_at,
            ..
        } = self.state
        else {
            return transactions.min();
        };
        let permissions = self
            .permissions
            .values()
            .filter(|p| !p.pending)
            .map(|p| p.refresh_at);
        let channels = self
            .channels
            .values()
            .filter(|c| !c.pending)
            .map(|c| c.refresh_at);
        transactions
            .chain(permissions)
            .chain(channels)
            .chain([refresh_at, expires_at])
            .min()
    }

    /// Makes `peer` reachable through the relay: a permission for its IP and a
    /// channel for its address. Requests wait for the allocation if needed.
    pub fn add_peer(&mut self, peer: SocketAddr, now: Instant) {
        if self.state == State::Failed {
            return;
        }
        if let Some(relayed) = self.relayed() {
            // A relay only reaches peers of its own family.
            if relayed.is_ipv4() != peer.is_ipv4() {
                return;
            }
        }
        self.permissions
            .entry(peer.ip())
            .or_insert_with(|| Permission {
                installed: false,
                pending: false,
                refresh_at: now,
            });
        if !self.channels.contains_key(&peer) && self.next_channel <= LAST_CHANNEL {
            let number = self.next_channel;
            self.next_channel += 1;
            self.channels.insert(
                peer,
                Channel {
                    number,
                    bound: false,
                    pending: false,
                    refresh_at: now,
                },
            );
        }
        self.handle_timeout(now);
    }

    /// True when `peer` was added with [`Self::add_peer`].
    pub fn knows_peer(&self, peer: SocketAddr) -> bool {
        self.channels.contains_key(&peer) || self.permissions.contains_key(&peer.ip())
    }

    /// Wraps `payload` for `peer`: ChannelData when the channel is bound, a Send
    /// indication when only the permission is installed.
    ///
    /// Returns `None` while neither is ready; the server would drop the datagram
    /// anyway, and ICE retransmits its checks.
    pub fn encapsulate(&self, peer: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
        if self.relayed().is_none() || payload.len() > u16::MAX as usize {
            return None;
        }
        if let Some(channel) = self.channels.get(&peer).filter(|c| c.bound) {
            let mut frame = Vec::with_capacity(4 + payload.len());
            frame.extend_from_slice(&channel.number.to_be_bytes());
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            frame.extend_from_slice(payload);
            return Some(frame);
        }
        if self
            .permissions
            .get(&peer.ip())
            .is_some_and(|p| p.installed)
        {
            return Some(
                Builder::new(method::SEND, class::INDICATION)
                    .xor_peer_address(peer)
                    .data(payload)
                    .build(None, false),
            );
        }
        None
    }

    /// Releases the allocation (a Refresh with lifetime zero). Best effort.
    pub fn close(&mut self, now: Instant) {
        if self.relayed().is_some() {
            self.start(Request::Refresh { lifetime: 0 }, now, 0);
        }
        self.state = State::Failed;
    }

    /// Drives retransmissions, refreshes and expiry.
    pub fn handle_timeout(&mut self, now: Instant) {
        self.retransmit(now);

        let State::Ready {
            refresh_at,
            expires_at,
            ..
        } = self.state
        else {
            return;
        };
        if now >= expires_at {
            self.fail(437, "allocation expired".into());
            return;
        }
        if now >= refresh_at && !self.has_pending(|r| matches!(r, Request::Refresh { .. })) {
            // Pushed back until the response reschedules it.
            self.reschedule_refresh(now + RETRY_AFTER_ERROR);
            self.start(
                Request::Refresh {
                    lifetime: ALLOCATION_LIFETIME,
                },
                now,
                0,
            );
        }

        let due_permissions: Vec<IpAddr> = self
            .permissions
            .iter()
            .filter(|(_, p)| !p.pending && now >= p.refresh_at)
            .map(|(ip, _)| *ip)
            .collect();
        for ip in due_permissions {
            if let Some(permission) = self.permissions.get_mut(&ip) {
                permission.pending = true;
            }
            self.start(Request::CreatePermission(ip), now, 0);
        }

        let due_channels: Vec<(SocketAddr, u16)> = self
            .channels
            .iter()
            .filter(|(_, c)| !c.pending && now >= c.refresh_at)
            .map(|(peer, c)| (*peer, c.number))
            .collect();
        for (peer, number) in due_channels {
            if let Some(channel) = self.channels.get_mut(&peer) {
                channel.pending = true;
            }
            self.start(Request::ChannelBind { peer, number }, now, 0);
        }
    }

    /// Consumes a datagram received from the TURN server.
    pub fn handle_datagram<'a>(&mut self, bytes: &'a [u8], now: Instant) -> Received<'a> {
        if let Some((number, payload)) = parse_channel_data(bytes) {
            let peer = self
                .channels
                .iter()
                .find(|(_, channel)| channel.number == number)
                .map(|(peer, _)| *peer);
            return match peer {
                Some(peer) => Received::Peer {
                    peer,
                    payload: Cow::Borrowed(payload),
                },
                None => Received::Consumed,
            };
        }
        if !Message::is_stun(bytes) {
            return Received::NotTurn;
        }
        let Ok(message) = Message::parse(bytes) else {
            return Received::NotTurn;
        };
        if message.fingerprint == Some(false) {
            return Received::Consumed;
        }
        if message.class == class::INDICATION && message.method == method::DATA {
            return match (message.xor_peer_address, message.data) {
                (Some(peer), Some(data)) if self.knows_peer(peer) => Received::Peer {
                    peer,
                    payload: Cow::Owned(data),
                },
                _ => Received::Consumed,
            };
        }
        if message.class != class::SUCCESS && message.class != class::ERROR {
            return Received::NotTurn;
        }
        let Some(transaction) = self.transactions.get(&message.transaction_id) else {
            return Received::NotTurn;
        };
        if !self.authentic(transaction, &message) {
            // Forged or corrupted; let the retransmission timer continue.
            return Received::Consumed;
        }
        let transaction = self
            .transactions
            .remove(&message.transaction_id)
            .expect("transaction exists");
        match message.error_code.clone() {
            None if message.class == class::SUCCESS => self.on_success(transaction, &message, now),
            None => self.on_error(
                transaction,
                400,
                "error without a code".into(),
                &message,
                now,
            ),
            Some((code, reason)) => self.on_error(transaction, code, reason, &message, now),
        }
        Received::Consumed
    }

    /// Checks MESSAGE-INTEGRITY on a response to an authenticated request.
    ///
    /// Success responses must carry a valid one. Error responses are accepted
    /// without one, because 401 and 438 are unauthenticated by design, but a
    /// present and wrong value is rejected.
    fn authentic(&self, transaction: &Transaction, message: &Message) -> bool {
        if !transaction.authenticated {
            return true;
        }
        let Some(key) = &self.key else { return false };
        match &message.integrity {
            Some(_) => message.verify_integrity(key).is_ok(),
            None => message.class == class::ERROR,
        }
    }

    fn on_success(&mut self, transaction: Transaction, message: &Message, now: Instant) {
        match transaction.request {
            Request::Allocate => {
                if self.state != State::Allocating {
                    return;
                }
                let Some(relayed) = message.xor_relayed_address else {
                    self.fail(500, "allocate response without a relayed address".into());
                    return;
                };
                let lifetime = message.lifetime.unwrap_or(ALLOCATION_LIFETIME);
                self.state = State::Ready {
                    relayed,
                    refresh_at: now + refresh_delay(lifetime),
                    expires_at: now + Duration::from_secs(lifetime as u64),
                };
                // Peers of the other family cannot be reached through this relay.
                self.permissions
                    .retain(|ip, _| ip.is_ipv4() == relayed.is_ipv4());
                self.channels
                    .retain(|peer, _| peer.is_ipv4() == relayed.is_ipv4());
                self.events.push_back(Event::Allocated(relayed));
                // Peers added while allocating are due now.
                self.handle_timeout(now);
            }
            Request::Refresh { lifetime: 0 } => {}
            Request::Refresh { .. } => {
                let lifetime = message.lifetime.unwrap_or(ALLOCATION_LIFETIME);
                if let State::Ready {
                    refresh_at,
                    expires_at,
                    ..
                } = &mut self.state
                {
                    *refresh_at = now + refresh_delay(lifetime);
                    *expires_at = now + Duration::from_secs(lifetime as u64);
                }
            }
            Request::CreatePermission(ip) => {
                if let Some(permission) = self.permissions.get_mut(&ip) {
                    permission.installed = true;
                    permission.pending = false;
                    permission.refresh_at = now + PERMISSION_REFRESH;
                }
            }
            Request::ChannelBind { peer, .. } => {
                if let Some(channel) = self.channels.get_mut(&peer) {
                    channel.bound = true;
                    channel.pending = false;
                    channel.refresh_at = now + CHANNEL_REFRESH;
                }
                // A channel binding installs the permission as well.
                if let Some(permission) = self.permissions.get_mut(&peer.ip()) {
                    permission.installed = true;
                }
            }
        }
    }

    fn on_error(
        &mut self,
        transaction: Transaction,
        code: u16,
        reason: String,
        message: &Message,
        now: Instant,
    ) {
        let request = transaction.request;
        match code {
            401 if request == Request::Allocate && !transaction.authenticated => {
                let (Some(realm), Some(nonce)) = (message.realm.clone(), message.nonce.clone())
                else {
                    self.fail(code, "401 without realm and nonce".into());
                    return;
                };
                self.key = Some(stun::long_term_key(&self.username, &realm, &self.password));
                self.realm = Some(realm);
                self.nonce = Some(nonce);
                self.start(Request::Allocate, now, 0);
                return;
            }
            438 if transaction.stale_nonces < MAX_STALE_NONCE => {
                if let Some(nonce) = message.nonce.clone() {
                    self.nonce = Some(nonce);
                }
                if let Some(realm) = message.realm.clone() {
                    if self.realm.as_deref() != Some(realm.as_str()) {
                        self.key =
                            Some(stun::long_term_key(&self.username, &realm, &self.password));
                        self.realm = Some(realm);
                    }
                }
                self.start(request, now, transaction.stale_nonces + 1);
                return;
            }
            _ => {}
        }
        self.request_failed(request, code, reason, now);
    }

    fn request_failed(&mut self, request: Request, code: u16, reason: String, now: Instant) {
        match request {
            Request::Allocate => self.fail(code, reason),
            Request::Refresh { lifetime: 0 } => {}
            // 437: the server no longer knows the allocation.
            Request::Refresh { .. } if code == 437 => self.fail(code, reason),
            Request::Refresh { .. } => self.reschedule_refresh(now + RETRY_AFTER_ERROR),
            Request::CreatePermission(ip) => {
                if let Some(permission) = self.permissions.get_mut(&ip) {
                    permission.pending = false;
                    permission.refresh_at = now + RETRY_AFTER_ERROR;
                }
            }
            Request::ChannelBind { peer, .. } => {
                if let Some(channel) = self.channels.get_mut(&peer) {
                    channel.pending = false;
                    channel.refresh_at = now + RETRY_AFTER_ERROR;
                }
            }
        }
    }

    fn reschedule_refresh(&mut self, at: Instant) {
        if let State::Ready { refresh_at, .. } = &mut self.state {
            *refresh_at = at;
        }
    }

    fn has_pending(&self, predicate: impl Fn(&Request) -> bool) -> bool {
        self.transactions.values().any(|t| predicate(&t.request))
    }

    fn retransmit(&mut self, now: Instant) {
        let mut timed_out = Vec::new();
        for (id, transaction) in self.transactions.iter_mut() {
            if now < transaction.deadline {
                continue;
            }
            if transaction.transmissions >= MAX_TRANSMISSIONS {
                timed_out.push(*id);
                continue;
            }
            self.outgoing.push_back(transaction.bytes.clone());
            transaction.transmissions += 1;
            transaction.rto *= 2;
            transaction.deadline = if transaction.transmissions >= MAX_TRANSMISSIONS {
                now + INITIAL_RTO * FINAL_WAIT_FACTOR
            } else {
                now + transaction.rto
            };
        }
        for id in timed_out {
            if let Some(transaction) = self.transactions.remove(&id) {
                self.request_failed(transaction.request, 408, "request timed out".into(), now);
            }
        }
    }

    /// Builds, records and queues a request.
    fn start(&mut self, request: Request, now: Instant, stale_nonces: u32) {
        let method = match request {
            Request::Allocate => method::ALLOCATE,
            Request::Refresh { .. } => method::REFRESH,
            Request::CreatePermission(_) => method::CREATE_PERMISSION,
            Request::ChannelBind { .. } => method::CHANNEL_BIND,
        };
        let mut builder = Builder::new(method, class::REQUEST);
        builder = match request {
            Request::Allocate => builder
                .requested_transport_udp()
                .lifetime(ALLOCATION_LIFETIME),
            Request::Refresh { lifetime } => builder.lifetime(lifetime),
            Request::CreatePermission(ip) => builder.xor_peer_address(SocketAddr::new(ip, 0)),
            Request::ChannelBind { peer, number } => {
                builder.channel_number(number).xor_peer_address(peer)
            }
        };
        let credentials = match (&self.realm, &self.nonce, &self.key) {
            (Some(realm), Some(nonce), Some(key)) => Some((realm, nonce, key)),
            _ => None,
        };
        let id = builder.id();
        let authenticated = credentials.is_some();
        let bytes = match credentials {
            Some((realm, nonce, key)) => builder
                .username(&self.username)
                .realm(realm)
                .nonce(nonce)
                .build(Some(key), true),
            None => builder.build(None, true),
        };
        self.outgoing.push_back(bytes.clone());
        self.transactions.insert(
            id,
            Transaction {
                request,
                bytes,
                authenticated,
                deadline: now + INITIAL_RTO,
                rto: INITIAL_RTO,
                transmissions: 1,
                stale_nonces,
            },
        );
    }

    fn fail(&mut self, code: u16, reason: String) {
        if self.state == State::Failed {
            return;
        }
        self.state = State::Failed;
        self.transactions.clear();
        self.failure = Some((code, reason.clone()));
        self.events.push_back(Event::Failed { code, reason });
    }
}

/// Refresh a little before the lifetime runs out, but never in a tight loop.
fn refresh_delay(lifetime: u32) -> Duration {
    let lifetime = lifetime as u64;
    Duration::from_secs(lifetime.saturating_sub(60).max(lifetime / 2).max(1))
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

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "203.0.113.1:3478";
    const RELAYED: &str = "198.51.100.7:50000";

    fn key() -> Vec<u8> {
        stun::long_term_key("user", "waystone", "pass")
    }

    fn sent(client: &mut TurnClient) -> Vec<Message> {
        std::iter::from_fn(|| client.poll_transmit())
            .map(|bytes| Message::parse(&bytes).unwrap())
            .collect()
    }

    fn respond(request: &Message, class: u16) -> Builder {
        Builder::new(request.method, class).transaction_id(request.transaction_id)
    }

    /// Runs the 401 challenge and returns the authenticated Allocate request.
    fn challenged(now: Instant) -> (TurnClient, Message) {
        let mut client = TurnClient::new(SERVER.parse().unwrap(), "user", "pass", now);
        let probe = sent(&mut client).remove(0);
        assert_eq!(probe.method, method::ALLOCATE);
        assert!(probe.integrity.is_none());
        let challenge = respond(&probe, class::ERROR)
            .error_code(401, "Unauthorized")
            .realm("waystone")
            .nonce("nonce-1")
            .build(None, true);
        assert_eq!(client.handle_datagram(&challenge, now), Received::Consumed);
        let retry = sent(&mut client).remove(0);
        (client, retry)
    }

    fn allocated(now: Instant) -> TurnClient {
        let (mut client, request) = challenged(now);
        let response = respond(&request, class::SUCCESS)
            .xor_relayed_address(RELAYED.parse().unwrap())
            .lifetime(600)
            .build(Some(&key()), true);
        assert_eq!(client.handle_datagram(&response, now), Received::Consumed);
        assert_eq!(
            client.poll_event(),
            Some(Event::Allocated(RELAYED.parse().unwrap()))
        );
        client
    }

    #[test]
    fn challenge_then_allocation() {
        let now = Instant::now();
        let (_, request) = challenged(now);
        assert_eq!(request.username.as_deref(), Some("user"));
        assert_eq!(request.realm.as_deref(), Some("waystone"));
        assert_eq!(request.lifetime, Some(ALLOCATION_LIFETIME));
        request.verify_integrity(&key()).unwrap();
        let client = allocated(now);
        assert_eq!(client.relayed(), Some(RELAYED.parse().unwrap()));
    }

    #[test]
    fn success_without_valid_integrity_is_ignored() {
        let now = Instant::now();
        let (mut client, request) = challenged(now);
        let forged = respond(&request, class::SUCCESS)
            .xor_relayed_address(RELAYED.parse().unwrap())
            .build(Some(b"not the key"), true);
        client.handle_datagram(&forged, now);
        let unsigned = respond(&request, class::SUCCESS)
            .xor_relayed_address(RELAYED.parse().unwrap())
            .build(None, true);
        client.handle_datagram(&unsigned, now);
        assert_eq!(client.relayed(), None);
        assert_eq!(client.poll_event(), None);
    }

    #[test]
    fn requests_are_retransmitted_then_fail() {
        let start = Instant::now();
        let mut client = TurnClient::new(SERVER.parse().unwrap(), "user", "pass", start);
        let first = sent(&mut client).remove(0);

        let mut now = start;
        let mut transmissions = 1;
        while client.poll_event().is_none() {
            now = client.poll_timeout().unwrap();
            client.handle_timeout(now);
            for retry in sent(&mut client) {
                assert_eq!(retry.transaction_id, first.transaction_id);
                transmissions += 1;
            }
        }
        assert_eq!(transmissions, MAX_TRANSMISSIONS);
        // 0.5 + 1 + 2 + 4 + 8 + 16 s of back-off, then the 8 s final wait.
        assert_eq!(now - start, Duration::from_millis(39_500));
        assert_eq!(client.failure().unwrap().0, 408);
    }

    #[test]
    fn concurrent_requests_do_not_clobber_each_other() {
        let now = Instant::now();
        let mut client = allocated(now);
        let first: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        let second: SocketAddr = "192.0.2.11:62000".parse().unwrap();
        client.add_peer(first, now);
        client.add_peer(second, now);
        let requests = sent(&mut client);
        let binds: Vec<&Message> = requests
            .iter()
            .filter(|m| m.method == method::CHANNEL_BIND)
            .collect();
        let permissions = requests
            .iter()
            .filter(|m| m.method == method::CREATE_PERMISSION)
            .count();
        assert_eq!(binds.len(), 2);
        assert_eq!(permissions, 2);
        assert_ne!(binds[0].channel_number, binds[1].channel_number);

        // Answer in reverse order; both channels must end up bound.
        for bind in binds.iter().rev() {
            let ok = respond(bind, class::SUCCESS).build(Some(&key()), false);
            client.handle_datagram(&ok, now);
        }
        for peer in [first, second] {
            let frame = client.encapsulate(peer, b"hello").unwrap();
            let (number, payload) = parse_channel_data(&frame).unwrap();
            assert_eq!(payload, b"hello");
            let echoed = client.handle_datagram(&frame, now);
            assert_eq!(
                echoed,
                Received::Peer {
                    peer,
                    payload: Cow::Borrowed(&b"hello"[..])
                }
            );
            assert!(number >= FIRST_CHANNEL);
        }
    }

    #[test]
    fn send_indication_is_used_until_the_channel_is_bound() {
        let now = Instant::now();
        let mut client = allocated(now);
        let peer: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        assert!(client.encapsulate(peer, b"x").is_none());
        client.add_peer(peer, now);
        let requests = sent(&mut client);
        assert!(client.encapsulate(peer, b"x").is_none());

        let permission = requests
            .iter()
            .find(|m| m.method == method::CREATE_PERMISSION)
            .unwrap();
        assert_eq!(permission.xor_peer_address.unwrap().ip(), peer.ip());
        let ok = respond(permission, class::SUCCESS).build(Some(&key()), false);
        client.handle_datagram(&ok, now);

        let indication = Message::parse(&client.encapsulate(peer, b"x").unwrap()).unwrap();
        assert_eq!(indication.method, method::SEND);
        assert_eq!(indication.class, class::INDICATION);
        assert_eq!(indication.xor_peer_address, Some(peer));
        assert_eq!(indication.data.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn data_indication_is_unwrapped() {
        let now = Instant::now();
        let mut client = allocated(now);
        let peer: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        client.add_peer(peer, now);
        let data = Builder::new(method::DATA, class::INDICATION)
            .xor_peer_address(peer)
            .data(b"ping")
            .build(None, false);
        assert_eq!(
            client.handle_datagram(&data, now),
            Received::Peer {
                peer,
                payload: Cow::Owned(b"ping".to_vec())
            }
        );
        // Unknown peers are dropped.
        let stranger = Builder::new(method::DATA, class::INDICATION)
            .xor_peer_address("192.0.2.99:1".parse().unwrap())
            .data(b"ping")
            .build(None, false);
        assert_eq!(client.handle_datagram(&stranger, now), Received::Consumed);
    }

    #[test]
    fn stale_nonce_is_retried_with_the_new_nonce() {
        let now = Instant::now();
        let mut client = allocated(now);
        let peer: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        client.add_peer(peer, now);
        let bind = sent(&mut client)
            .into_iter()
            .find(|m| m.method == method::CHANNEL_BIND)
            .unwrap();
        let stale = respond(&bind, class::ERROR)
            .error_code(438, "Stale Nonce")
            .realm("waystone")
            .nonce("nonce-2")
            .build(None, false);
        client.handle_datagram(&stale, now);
        let retry = sent(&mut client).remove(0);
        assert_eq!(retry.method, method::CHANNEL_BIND);
        assert_ne!(retry.transaction_id, bind.transaction_id);
        assert_eq!(retry.nonce.as_deref(), Some("nonce-2"));
        assert_eq!(retry.channel_number, bind.channel_number);
        retry.verify_integrity(&key()).unwrap();
    }

    #[test]
    fn refreshes_are_scheduled() {
        let now = Instant::now();
        let mut client = allocated(now);
        let peer: SocketAddr = "192.0.2.10:61000".parse().unwrap();
        client.add_peer(peer, now);
        for request in sent(&mut client) {
            let ok = respond(&request, class::SUCCESS).build(Some(&key()), false);
            client.handle_datagram(&ok, now);
        }

        // Permission refresh at four minutes.
        let at = client.poll_timeout().unwrap();
        assert_eq!(at - now, PERMISSION_REFRESH);
        client.handle_timeout(at);
        let requests = sent(&mut client);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, method::CREATE_PERMISSION);
        let ok = respond(&requests[0], class::SUCCESS).build(Some(&key()), false);
        client.handle_datagram(&ok, at);

        // Second permission refresh at eight minutes.
        let at = client.poll_timeout().unwrap();
        assert_eq!(at - now, PERMISSION_REFRESH * 2);
        client.handle_timeout(at);
        let requests = sent(&mut client);
        assert_eq!(requests[0].method, method::CREATE_PERMISSION);
        let ok = respond(&requests[0], class::SUCCESS).build(Some(&key()), false);
        client.handle_datagram(&ok, at);

        // Allocation refresh a minute before the 600 s lifetime ends, together
        // with the channel refresh at nine minutes.
        let at = client.poll_timeout().unwrap();
        assert_eq!(at - now, CHANNEL_REFRESH);
        client.handle_timeout(at);
        let methods: Vec<u16> = sent(&mut client).iter().map(|m| m.method).collect();
        assert_eq!(methods, vec![method::REFRESH, method::CHANNEL_BIND]);
    }

    #[test]
    fn reports_server_failure() {
        let now = Instant::now();
        let (mut client, request) = challenged(now);
        let failure = respond(&request, class::ERROR)
            .error_code(486, "Allocation Quota Reached")
            .build(Some(&key()), false);
        client.handle_datagram(&failure, now);
        assert_eq!(
            client.poll_event(),
            Some(Event::Failed {
                code: 486,
                reason: "Allocation Quota Reached".into()
            })
        );
    }

    #[test]
    fn rejected_credentials_fail_instead_of_looping() {
        let now = Instant::now();
        let (mut client, request) = challenged(now);
        let again = respond(&request, class::ERROR)
            .error_code(401, "Unauthorized")
            .realm("waystone")
            .nonce("nonce-1")
            .build(None, false);
        client.handle_datagram(&again, now);
        assert!(matches!(
            client.poll_event(),
            Some(Event::Failed { code: 401, .. })
        ));
        assert!(client.poll_transmit().is_none());
    }

    #[test]
    fn ignores_foreign_datagrams() {
        let now = Instant::now();
        let (mut client, _) = challenged(now);
        assert_eq!(
            client.handle_datagram(&[0x80, 0x60, 0, 1], now),
            Received::NotTurn
        );
        let foreign = Builder::new(method::ALLOCATE, class::SUCCESS).build(None, true);
        assert_eq!(client.handle_datagram(&foreign, now), Received::NotTurn);
    }
}
