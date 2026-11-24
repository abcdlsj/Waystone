//! Signaling hub: authenticates devices and routes messages between them.
//!
//! The hub owns no sockets. Every connection registers a [`Connection`] (a
//! bounded outbox plus a kick switch) and the WebSocket task drains it, so the
//! routing and session rules can be unit tested without any network.
//!
//! Locking rules:
//!
//! * `inner` guards the in-memory registry and is never held across SQLite
//!   calls or signature checks; the device pool is cached inside it and
//!   refreshed whenever the pool changes (pairing, revocation, rename).
//! * `refresh` serializes cache refreshes so two concurrent reloads cannot
//!   install their snapshots out of order.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use waystone_proto::device::{DeviceId, DeviceInfo, Role, Viewport};
use waystone_proto::identity::Identity;
use waystone_proto::signal::{
    version_compatible, ByeReason, ClientMessage, PeerState, ServerMessage, TurnCredentials,
};
use waystone_proto::VideoCodec;

use crate::challenge::ChallengeStore;
use crate::store::Store;

/// Messages queued for one connection before it counts as a slow consumer.
///
/// Signaling is bursty (a trickle of ICE candidates) but tiny; a client that
/// lets this many messages pile up is not reading its socket and gets dropped
/// instead of growing the daemon's memory without bound.
pub const OUTBOX_CAPACITY: usize = 256;

/// How long relay credentials minted at authentication stay valid.
pub const TURN_CREDENTIAL_TTL: Duration = Duration::from_secs(12 * 3600);

/// The hub's handle on one WebSocket.
#[derive(Clone)]
pub struct Connection {
    /// Messages for the socket task to write.
    pub outbox: Sender<ServerMessage>,
    /// Cancelled when the hub wants the socket closed. The socket task flushes
    /// what is already queued (so a final `AuthRejected` still arrives) and
    /// then closes.
    pub kick: CancellationToken,
}

impl Connection {
    /// Queues `message`; a full queue marks the consumer as too slow and kicks it.
    fn deliver(&self, message: ServerMessage) {
        match self.outbox.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                warn!("signaling outbox full, dropping the slow connection");
                self.kick.cancel();
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }

    /// Delivers a last message and asks the socket task to close.
    fn close_with(&self, message: ServerMessage) {
        self.deliver(message);
        self.kick.cancel();
    }
}

/// Where a connection stands in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Waiting for `Auth`; `paired` once a `PairRequest` succeeded on it.
    Pending { paired: bool },
    /// Authenticated as a device.
    Authenticated,
    /// Unknown or already disconnected.
    Gone,
}

/// Live state of one device, for the admin panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub busy: bool,
    pub rtt_ms: Option<u32>,
}

/// Relay settings used to mint per-device TURN credentials.
#[derive(Clone)]
pub struct TurnConfig {
    /// STUN/TURN authority handed to clients, e.g. `192.168.1.10:3478`.
    pub server: String,
    /// Shared secret the TURN server validates credentials with.
    pub secret: String,
    /// Lifetime of minted credentials.
    pub ttl: Duration,
    /// Bitrate clamp for relayed sessions; `None` disables it.
    pub relay_bitrate_ceiling: Option<u32>,
}

/// Shared state of the signaling layer.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    refresh: Arc<Mutex<()>>,
    store: Arc<Store>,
    challenges: Arc<ChallengeStore>,
    server_id: Arc<str>,
    turn: Option<Arc<TurnConfig>>,
}

struct Pending {
    link: Connection,
    paired: bool,
}

/// One authenticated device.
struct Peer {
    connection: u64,
    info: DeviceInfo,
    link: Connection,
    session: Option<String>,
    rtt_ms: Option<u32>,
}

/// A session between a host and a controller.
struct Session {
    host: DeviceId,
    controller: DeviceId,
}

#[derive(Default)]
struct HubInner {
    pending: HashMap<u64, Pending>,
    /// Authenticated connection id to device id.
    authed: HashMap<u64, DeviceId>,
    peers: HashMap<DeviceId, Peer>,
    sessions: HashMap<String, Session>,
    /// Cached device pool; the source of truth stays in SQLite.
    devices: HashMap<DeviceId, DeviceInfo>,
}

impl HubInner {
    fn peer_by_connection(&self, connection_id: u64) -> Option<&Peer> {
        let id = self.authed.get(&connection_id)?;
        self.peers
            .get(id)
            .filter(|peer| peer.connection == connection_id)
    }

    fn send_to_connection(&self, connection_id: u64, message: ServerMessage) {
        if let Some(pending) = self.pending.get(&connection_id) {
            pending.link.deliver(message);
        } else if let Some(peer) = self.peer_by_connection(connection_id) {
            peer.link.deliver(message);
        }
    }

    /// Ends `session_id`, telling every remaining participant why.
    fn end_session(&mut self, session_id: &str, reason: ByeReason, detail: Option<String>) {
        let Some(session) = self.sessions.remove(session_id) else {
            return;
        };
        info!("session {session_id} ended: {reason:?}");
        for id in [session.host, session.controller] {
            let Some(peer) = self.peers.get_mut(&id) else {
                continue;
            };
            if peer.session.as_deref() != Some(session_id) {
                continue;
            }
            peer.session = None;
            peer.link.deliver(ServerMessage::SessionEnded {
                session_id: session_id.to_string(),
                reason,
                detail: detail.clone(),
            });
        }
    }

    /// Drops a peer from the registry and ends its session with `detail`.
    fn remove_peer(&mut self, id: &DeviceId, detail: String) -> Option<Peer> {
        let mut peer = self.peers.remove(id)?;
        self.authed.remove(&peer.connection);
        if let Some(session_id) = peer.session.take() {
            self.end_session(&session_id, ByeReason::PeerOffline, Some(detail));
        }
        Some(peer)
    }

    /// Every device in the pool with its live state, hosts first then by name
    /// so the phone list does not jump around.
    fn snapshot(&self) -> Vec<PeerState> {
        let mut list: Vec<PeerState> = self
            .devices
            .values()
            .map(|device| {
                let peer = self.peers.get(&device.id);
                PeerState {
                    device: device.clone(),
                    online: peer.is_some(),
                    rtt_ms: peer.and_then(|peer| peer.rtt_ms),
                    busy: peer.is_some_and(|peer| peer.session.is_some()),
                }
            })
            .collect();
        list.sort_by(|a, b| {
            let rank = |role: Role| if role == Role::Host { 0 } else { 1 };
            rank(a.device.role)
                .cmp(&rank(b.device.role))
                .then_with(|| a.device.name.cmp(&b.device.name))
                .then_with(|| a.device.id.as_str().cmp(b.device.id.as_str()))
        });
        list
    }

    fn peer_list(snapshot: &[PeerState], viewer: &DeviceId) -> Vec<PeerState> {
        snapshot
            .iter()
            .filter(|state| &state.device.id != viewer)
            .cloned()
            .collect()
    }

    /// Pushes the current pool to every connected device except `skip`.
    fn broadcast(&self, skip: Option<&DeviceId>) {
        let snapshot = self.snapshot();
        for (id, peer) in &self.peers {
            if Some(id) == skip {
                continue;
            }
            peer.link.deliver(ServerMessage::PeerUpdate {
                peers: Self::peer_list(&snapshot, id),
            });
        }
    }
}

impl Hub {
    /// Creates a hub for `server_id`, backed by `store`, and loads the device pool.
    pub fn new(server_id: impl Into<String>, store: Arc<Store>) -> Result<Self> {
        let devices = load_devices(&store)?;
        Ok(Hub {
            inner: Arc::new(Mutex::new(HubInner {
                devices,
                ..HubInner::default()
            })),
            refresh: Arc::new(Mutex::new(())),
            store,
            challenges: Arc::new(ChallengeStore::new()),
            server_id: Arc::from(server_id.into()),
            turn: None,
        })
    }

    /// Enables per-device relay credentials in `AuthOk`.
    pub fn with_turn(mut self, turn: Option<TurnConfig>) -> Self {
        self.turn = turn.map(Arc::new);
        self
    }

    fn lock(&self) -> MutexGuard<'_, HubInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a new connection.
    ///
    /// The first message the caller sends is the challenge from [`Hub::challenge`].
    pub fn connect(&self, link: Connection) -> u64 {
        let connection_id = self.challenges.next_connection();
        self.lock().pending.insert(
            connection_id,
            Pending {
                link,
                paired: false,
            },
        );
        connection_id
    }

    /// Issues the per connection challenge.
    ///
    /// Signatures are only valid for one connection, so an attacker who records a
    /// handshake cannot replay it.
    pub fn challenge(&self, connection_id: u64) -> ServerMessage {
        let challenge = self.challenges.issue(connection_id, &self.server_id);
        ServerMessage::Challenge {
            nonce: challenge.nonce,
            server_id: challenge.server_id,
            protocol_version: waystone_proto::PROTOCOL_VERSION,
        }
    }

    /// Where `connection_id` stands in the handshake.
    pub fn connection_state(&self, connection_id: u64) -> ConnectionState {
        let inner = self.lock();
        if let Some(pending) = inner.pending.get(&connection_id) {
            ConnectionState::Pending {
                paired: pending.paired,
            }
        } else if inner.peer_by_connection(connection_id).is_some() {
            ConnectionState::Authenticated
        } else {
            ConnectionState::Gone
        }
    }

    /// Handles one client message coming from `connection_id`.
    pub fn handle(&self, connection_id: u64, message: ClientMessage) -> Result<()> {
        match message {
            ClientMessage::PairRequest { token, device } => {
                self.handle_pair(connection_id, &token, device)
            }
            ClientMessage::Auth {
                identity,
                signature,
                protocol_version,
                name,
            } => {
                self.handle_auth(connection_id, identity, &signature, protocol_version, &name);
                Ok(())
            }
            ClientMessage::SessionStart {
                target,
                viewport,
                audio,
                video_codec,
            } => {
                self.handle_session_start(connection_id, target, viewport, audio, video_codec);
                Ok(())
            }
            ClientMessage::RtcSignal { session_id, signal } => {
                let message = ServerMessage::RtcSignal {
                    session_id: session_id.clone(),
                    signal,
                };
                self.relay(connection_id, &session_id, message);
                Ok(())
            }
            ClientMessage::SessionControl {
                session_id,
                control,
            } => {
                let message = ServerMessage::SessionControl {
                    session_id: session_id.clone(),
                    control,
                };
                self.relay(connection_id, &session_id, message);
                Ok(())
            }
            ClientMessage::SessionBye { session_id, reason } => {
                self.handle_bye(connection_id, &session_id, reason);
                Ok(())
            }
            ClientMessage::Ping { nonce } => {
                self.lock()
                    .send_to_connection(connection_id, ServerMessage::Pong { nonce });
                Ok(())
            }
        }
    }

    /// Removes a connection and ends the session it participated in.
    pub fn disconnect(&self, connection_id: u64) {
        self.challenges.forget(connection_id);
        let mut inner = self.lock();
        inner.pending.remove(&connection_id);
        let Some(id) = inner.authed.remove(&connection_id) else {
            return;
        };
        // A replaced connection no longer owns the peer entry.
        if inner.peers.get(&id).map(|peer| peer.connection) != Some(connection_id) {
            return;
        }
        let name = inner.peers[&id].info.name.clone();
        inner.remove_peer(&id, format!("{name} went offline"));
        info!("device {name} ({id}) disconnected");
        inner.broadcast(None);
    }

    /// Records a WebSocket ping round trip for `connection_id`.
    ///
    /// Peers are only notified when the value moved enough to matter on screen;
    /// small jitter rides along with the next update instead.
    pub fn record_rtt(&self, connection_id: u64, rtt: Duration) {
        let rtt_ms = u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX);
        let mut inner = self.lock();
        let Some(id) = inner.authed.get(&connection_id).cloned() else {
            return;
        };
        let Some(peer) = inner.peers.get_mut(&id) else {
            return;
        };
        let previous = peer.rtt_ms.replace(rtt_ms);
        if rtt_changed_meaningfully(previous, rtt_ms) {
            inner.broadcast(Some(&id));
        }
    }

    /// Peers the given device may see.
    pub fn peer_list(&self, viewer: &DeviceId) -> Vec<PeerState> {
        let inner = self.lock();
        HubInner::peer_list(&inner.snapshot(), viewer)
    }

    /// Number of connected devices, for the admin panel.
    pub fn connected_count(&self) -> usize {
        self.lock().peers.len()
    }

    /// Live state of every connected device, for the admin panel.
    pub fn presence(&self) -> HashMap<DeviceId, Presence> {
        self.lock()
            .peers
            .iter()
            .map(|(id, peer)| {
                (
                    id.clone(),
                    Presence {
                        busy: peer.session.is_some(),
                        rtt_ms: peer.rtt_ms,
                    },
                )
            })
            .collect()
    }

    /// Removes a device from the pool, closing its connection and session.
    ///
    /// Returns `false` when the device was not paired.
    pub fn revoke(&self, id: &DeviceId) -> Result<bool> {
        if !self.store.remove_device(id)? {
            return Ok(false);
        }
        info!("revoked device {id}");
        self.refresh_devices()?;
        Ok(true)
    }

    /// Reloads the device pool from the store.
    ///
    /// Connected devices that are no longer in the pool are kicked; renamed or
    /// re-paired ones pick up their new description.
    pub fn refresh_devices(&self) -> Result<()> {
        let _serial = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let devices = load_devices(&self.store)?;
        let mut inner = self.lock();
        inner.devices = devices;

        let removed: Vec<DeviceId> = inner
            .peers
            .keys()
            .filter(|id| !inner.devices.contains_key(id))
            .cloned()
            .collect();
        for id in removed {
            let name = inner.peers[&id].info.name.clone();
            if let Some(peer) = inner.remove_peer(&id, format!("{name} was removed")) {
                peer.link.close_with(ServerMessage::AuthRejected {
                    reason: "this device was removed from the instance".into(),
                });
            }
        }
        let HubInner { peers, devices, .. } = &mut *inner;
        for (id, peer) in peers.iter_mut() {
            if let Some(info) = devices.get(id) {
                peer.info = info.clone();
            }
        }
        inner.broadcast(None);
        Ok(())
    }

    fn handle_pair(&self, connection_id: u64, token: &str, mut device: DeviceInfo) -> Result<()> {
        if device.public_key.is_empty() || device.name.is_empty() {
            self.reject_pair(connection_id, "device description is incomplete");
            return Ok(());
        }

        // The id is derived from the key, so a device can never claim an identity
        // it does not own.
        let identity = Identity {
            id: device.id.clone(),
            public_key: device.public_key.clone(),
        };
        if !identity.validate() {
            self.reject_pair(connection_id, "device id does not match its public key");
            return Ok(());
        }

        let Some(role) = self.store.consume_pairing_token(token)? else {
            self.reject_pair(
                connection_id,
                "pairing token is invalid, expired or already used",
            );
            return Ok(());
        };
        // The role comes from the token, not from the device: a phone must not
        // be able to enroll itself as a host.
        device.role = role;
        self.store.upsert_device(&device)?;
        info!("paired {} ({}) as {}", device.name, device.id, role);
        self.refresh_devices()?;

        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(&connection_id) {
            pending.paired = true;
        }
        inner.send_to_connection(
            connection_id,
            ServerMessage::PairAccepted {
                server_id: self.server_id.to_string(),
            },
        );
        Ok(())
    }

    fn reject_pair(&self, connection_id: u64, reason: &str) {
        self.lock().send_to_connection(
            connection_id,
            ServerMessage::PairRejected {
                reason: reason.to_string(),
            },
        );
    }

    fn handle_auth(
        &self,
        connection_id: u64,
        identity: Identity,
        signature: &str,
        protocol_version: u32,
        name: &str,
    ) {
        if !version_compatible(protocol_version) {
            return self.reject_auth(
                connection_id,
                format!("protocol version {protocol_version} is not supported"),
            );
        }
        let Some(challenge) = self.challenges.take(connection_id) else {
            return self.reject_auth(
                connection_id,
                "no challenge was issued for this connection".into(),
            );
        };

        // Signature checks run without the registry lock.
        let Some(public_key) = self
            .lock()
            .devices
            .get(&identity.id)
            .map(|device| device.public_key.clone())
        else {
            return self.reject_auth(
                connection_id,
                "device is not paired with this instance".into(),
            );
        };
        if public_key != identity.public_key {
            return self.reject_auth(
                connection_id,
                "public key does not match the paired device".into(),
            );
        }
        let Ok(signature_bytes) = base64_decode(signature) else {
            return self.reject_auth(connection_id, "signature is not valid base64".into());
        };
        if !crate::challenge::verify(&public_key, &challenge.signing_payload(), &signature_bytes) {
            warn!("rejected bad signature from {}", identity.id);
            return self.reject_auth(connection_id, "signature verification failed".into());
        }

        let (info, renamed) = {
            let mut inner = self.lock();
            // The device may have been revoked while the signature was checked.
            let Some(mut info) = inner
                .devices
                .get(&identity.id)
                .filter(|device| device.public_key == public_key)
                .cloned()
            else {
                drop(inner);
                return self.reject_auth(
                    connection_id,
                    "device is not paired with this instance".into(),
                );
            };
            let Some(pending) = inner.pending.remove(&connection_id) else {
                // The socket closed while we were verifying.
                return;
            };
            let renamed = !name.is_empty() && name != info.name;
            if renamed {
                info.name = name.to_string();
                inner.devices.insert(info.id.clone(), info.clone());
            }

            if let Some(previous) =
                inner.remove_peer(&info.id, format!("{} reconnected", info.name))
            {
                // The new connection wins; the stale socket is told why and closed.
                previous.link.close_with(ServerMessage::AuthRejected {
                    reason: "replaced by a newer connection from the same device".into(),
                });
            }
            inner.authed.insert(connection_id, info.id.clone());
            inner.peers.insert(
                info.id.clone(),
                Peer {
                    connection: connection_id,
                    info: info.clone(),
                    link: pending.link.clone(),
                    session: None,
                    rtt_ms: None,
                },
            );

            let peers = HubInner::peer_list(&inner.snapshot(), &info.id);
            pending.link.deliver(ServerMessage::AuthOk {
                peers,
                turn: self.mint_turn_credentials(),
            });
            inner.broadcast(Some(&info.id));
            (info, renamed)
        };
        info!("device {} ({}) authenticated", info.name, info.id);

        if renamed {
            if let Err(error) = self.store.upsert_device(&info) {
                warn!("persisting the new name of {} failed: {error}", info.id);
            }
        }
        if let Err(error) = self.store.touch_device(&info.id) {
            warn!("recording last seen for {} failed: {error}", info.id);
        }
    }

    fn mint_turn_credentials(&self) -> Option<TurnCredentials> {
        let turn = self.turn.as_ref()?;
        let (username, password) = crate::turnserver::credentials(&turn.secret, turn.ttl);
        Some(TurnCredentials {
            server: turn.server.clone(),
            username,
            password,
            relay_bitrate_ceiling: turn.relay_bitrate_ceiling,
        })
    }

    /// Rejects an authentication attempt and closes the connection, as the
    /// protocol promises.
    fn reject_auth(&self, connection_id: u64, reason: String) {
        let inner = self.lock();
        if let Some(pending) = inner.pending.get(&connection_id) {
            pending
                .link
                .close_with(ServerMessage::AuthRejected { reason });
        } else {
            inner.send_to_connection(connection_id, ServerMessage::Error { message: reason });
        }
    }

    fn handle_session_start(
        &self,
        connection_id: u64,
        target: DeviceId,
        viewport: Viewport,
        audio: bool,
        video_codec: VideoCodec,
    ) {
        let mut inner = self.lock();
        let reject = |inner: &HubInner, reason: &str| {
            inner.send_to_connection(
                connection_id,
                ServerMessage::SessionRejected {
                    reason: reason.to_string(),
                },
            );
        };
        let Some(controller) = inner.peer_by_connection(connection_id) else {
            inner.send_to_connection(
                connection_id,
                ServerMessage::Error {
                    message: "authenticate before starting a session".into(),
                },
            );
            return;
        };
        if controller.info.role != Role::Controller {
            return reject(&inner, "only a controller can start a session");
        }
        let controller_info = controller.info.clone();

        // One session per controller: starting a new one ends the old one.
        let previous = controller.session.clone();
        if let Some(previous) = &previous {
            inner.end_session(
                previous,
                ByeReason::UserHangup,
                Some(format!("{} started a new session", controller_info.name)),
            );
        }
        let refuse = |inner: &HubInner, reason: &str| {
            if previous.is_some() {
                inner.broadcast(None);
            }
            reject(inner, reason);
        };

        let Some(host) = inner.peers.get(&target) else {
            return refuse(&inner, "the host is offline");
        };
        if host.info.role != Role::Host {
            return refuse(&inner, "the target device is not a host");
        }
        if let Some(busy_with) = host.session.as_ref().and_then(|id| inner.sessions.get(id)) {
            let user = inner
                .peers
                .get(&busy_with.controller)
                .map(|peer| peer.info.name.clone())
                .unwrap_or_else(|| "another controller".into());
            return refuse(&inner, &format!("the host is in use by {user}"));
        }
        let host_info = host.info.clone();

        let mut raw = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut raw);
        let session_id = hex::encode(raw);
        inner.sessions.insert(
            session_id.clone(),
            Session {
                host: host_info.id.clone(),
                controller: controller_info.id.clone(),
            },
        );
        for id in [&host_info.id, &controller_info.id] {
            if let Some(peer) = inner.peers.get_mut(id) {
                peer.session = Some(session_id.clone());
            }
        }
        info!(
            "session {session_id}: {} -> {}",
            controller_info.name, host_info.name
        );

        let accepted = |is_host: bool, peer: DeviceInfo| ServerMessage::SessionAccepted {
            session_id: session_id.clone(),
            is_host,
            peer,
            viewport: Some(viewport),
            audio,
            video_codec,
        };
        inner.peers[&controller_info.id]
            .link
            .deliver(accepted(false, host_info.clone()));
        inner.peers[&host_info.id]
            .link
            .deliver(accepted(true, controller_info));
        inner.broadcast(None);
    }

    fn handle_bye(&self, connection_id: u64, session_id: &str, reason: ByeReason) {
        let mut inner = self.lock();
        let Some(sender) = inner.authed.get(&connection_id).cloned() else {
            return;
        };
        let participant = inner
            .sessions
            .get(session_id)
            .is_some_and(|session| session.host == sender || session.controller == sender);
        if !participant {
            debug!("ignoring bye for {session_id} from non-participant {sender}");
            return;
        }
        inner.end_session(session_id, reason, None);
        inner.broadcast(None);
    }

    fn relay(&self, connection_id: u64, session_id: &str, message: ServerMessage) {
        let inner = self.lock();
        let Some(sender) = inner.authed.get(&connection_id) else {
            return;
        };
        let Some(session) = inner.sessions.get(session_id) else {
            return;
        };
        let target = if *sender == session.host {
            &session.controller
        } else if *sender == session.controller {
            &session.host
        } else {
            // Not a participant: silently drop instead of leaking session state.
            return;
        };
        if let Some(peer) = inner.peers.get(target) {
            peer.link.deliver(message);
        }
    }
}

fn load_devices(store: &Store) -> Result<HashMap<DeviceId, DeviceInfo>> {
    Ok(store
        .devices()?
        .into_iter()
        .map(|stored| (stored.info.id.clone(), stored.info))
        .collect())
}

/// True when an RTT change is worth a peer update: the first sample, or a move
/// of at least 5 ms and 20 %.
fn rtt_changed_meaningfully(previous: Option<u32>, current: u32) -> bool {
    match previous {
        None => true,
        Some(previous) => {
            let delta = previous.abs_diff(current);
            delta >= 5 && delta.saturating_mul(5) >= previous
        }
    }
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| anyhow!("{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tokio::sync::mpsc::{channel, Receiver};
    use waystone_proto::device::Platform;
    use waystone_proto::identity::{encode_public_key, Challenge};

    struct TestDevice {
        signing: SigningKey,
        info: DeviceInfo,
    }

    impl TestDevice {
        fn new(name: &str, role: Role) -> TestDevice {
            let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
            let public = signing.verifying_key().to_bytes();
            TestDevice {
                signing,
                info: DeviceInfo {
                    id: DeviceId::from_public_key(&public),
                    name: name.into(),
                    platform: Platform::Ios,
                    role,
                    public_key: encode_public_key(&public),
                },
            }
        }
    }

    struct Socket {
        id: u64,
        rx: Receiver<ServerMessage>,
        kick: CancellationToken,
    }

    impl Socket {
        fn drain(&mut self) -> Vec<ServerMessage> {
            let mut out = Vec::new();
            while let Ok(message) = self.rx.try_recv() {
                out.push(message);
            }
            out
        }
    }

    fn hub() -> Hub {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store.ensure_instance("server", &"f".repeat(64)).unwrap();
        Hub::new("server", store).unwrap()
    }

    fn open(hub: &Hub) -> Socket {
        let (outbox, rx) = channel(OUTBOX_CAPACITY);
        let kick = CancellationToken::new();
        let id = hub.connect(Connection {
            outbox,
            kick: kick.clone(),
        });
        Socket { id, rx, kick }
    }

    /// Pairs (if needed) and authenticates `device` on a fresh connection.
    fn login(hub: &Hub, device: &TestDevice) -> Socket {
        if !hub.lock().devices.contains_key(&device.info.id) {
            hub.store.upsert_device(&device.info).unwrap();
            hub.refresh_devices().unwrap();
        }
        let mut socket = open(hub);
        let ServerMessage::Challenge {
            nonce, server_id, ..
        } = hub.challenge(socket.id)
        else {
            unreachable!()
        };
        use base64::Engine;
        let signature = device
            .signing
            .sign(&Challenge { nonce, server_id }.signing_payload());
        hub.handle(
            socket.id,
            ClientMessage::Auth {
                identity: Identity {
                    id: device.info.id.clone(),
                    public_key: device.info.public_key.clone(),
                },
                signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: device.info.name.clone(),
            },
        )
        .unwrap();
        assert!(
            socket
                .drain()
                .iter()
                .any(|message| matches!(message, ServerMessage::AuthOk { .. })),
            "authentication must succeed"
        );
        socket
    }

    fn start(hub: &Hub, socket: &Socket, host: &TestDevice) {
        hub.handle(
            socket.id,
            ClientMessage::SessionStart {
                target: host.info.id.clone(),
                viewport: Viewport {
                    width: 1,
                    height: 1,
                    dpi: 1,
                    fps: 60,
                },
                audio: false,
                video_codec: VideoCodec::H264,
            },
        )
        .unwrap();
    }

    fn accepted_session(messages: &[ServerMessage]) -> Option<String> {
        messages.iter().find_map(|message| match message {
            ServerMessage::SessionAccepted { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
    }

    #[test]
    fn busy_host_rejects_a_second_controller() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let bob = TestDevice::new("Bob", Role::Controller);
        let _host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);
        let mut bob_socket = login(&hub, &bob);

        start(&hub, &alice_socket, &host);
        assert!(accepted_session(&alice_socket.drain()).is_some());

        // Bob sees the host as busy.
        let busy = bob_socket
            .drain()
            .into_iter()
            .rev()
            .find_map(|message| match message {
                ServerMessage::PeerUpdate { peers } => peers
                    .into_iter()
                    .find(|peer| peer.device.id == host.info.id)
                    .map(|peer| peer.busy),
                _ => None,
            });
        assert_eq!(busy, Some(true));

        start(&hub, &bob_socket, &host);
        let reason = bob_socket
            .drain()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::SessionRejected { reason } => Some(reason),
                _ => None,
            });
        assert_eq!(reason.as_deref(), Some("the host is in use by Alice"));
    }

    #[test]
    fn a_new_session_ends_the_controllers_previous_one() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let mut host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);

        start(&hub, &alice_socket, &host);
        let first = accepted_session(&alice_socket.drain()).unwrap();
        host_socket.drain();

        // Restarting towards the same host must not be rejected as busy.
        start(&hub, &alice_socket, &host);
        let messages = alice_socket.drain();
        assert!(messages.iter().any(|message| matches!(
            message,
            ServerMessage::SessionEnded { session_id, .. } if *session_id == first
        )));
        let second = accepted_session(&messages).expect("second session starts");
        assert_ne!(first, second);
        assert!(host_socket.drain().iter().any(|message| matches!(
            message,
            ServerMessage::SessionEnded { session_id, .. } if *session_id == first
        )));
    }

    #[test]
    fn bye_from_a_non_participant_is_ignored() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let mallory = TestDevice::new("Mallory", Role::Controller);
        let mut host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);
        let mallory_socket = login(&hub, &mallory);

        start(&hub, &alice_socket, &host);
        let session = accepted_session(&alice_socket.drain()).unwrap();
        host_socket.drain();

        hub.handle(
            mallory_socket.id,
            ClientMessage::SessionBye {
                session_id: session.clone(),
                reason: ByeReason::UserHangup,
            },
        )
        .unwrap();
        let ended = |messages: Vec<ServerMessage>| {
            messages
                .iter()
                .any(|message| matches!(message, ServerMessage::SessionEnded { .. }))
        };
        assert!(!ended(alice_socket.drain()));
        assert!(!ended(host_socket.drain()));

        // The real participant can still hang up.
        hub.handle(
            alice_socket.id,
            ClientMessage::SessionBye {
                session_id: session,
                reason: ByeReason::UserHangup,
            },
        )
        .unwrap();
        assert!(ended(host_socket.drain()));
    }

    #[test]
    fn participant_disconnect_ends_the_session_as_peer_offline() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);
        start(&hub, &alice_socket, &host);
        alice_socket.drain();

        hub.disconnect(host_socket.id);
        let ended = alice_socket
            .drain()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::SessionEnded { reason, detail, .. } => Some((reason, detail)),
                _ => None,
            });
        assert_eq!(
            ended,
            Some((ByeReason::PeerOffline, Some("Mac went offline".into())))
        );
    }

    #[test]
    fn revocation_kicks_the_live_connection_and_ends_its_session() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let mut host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);
        start(&hub, &alice_socket, &host);
        host_socket.drain();

        assert!(hub.revoke(&alice.info.id).unwrap());
        assert!(alice_socket.kick.is_cancelled(), "socket must be closed");
        assert!(alice_socket
            .drain()
            .iter()
            .any(|message| matches!(message, ServerMessage::AuthRejected { .. })));
        let host_messages = host_socket.drain();
        assert!(host_messages.iter().any(|message| matches!(
            message,
            ServerMessage::SessionEnded {
                reason: ByeReason::PeerOffline,
                ..
            }
        )));
        assert_eq!(hub.connected_count(), 1);
        assert!(hub.peer_list(&host.info.id).is_empty());
        assert!(!hub.revoke(&alice.info.id).unwrap());
    }

    #[test]
    fn a_replaced_connection_is_rejected_and_closed() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let mut first = login(&hub, &host);
        let _second = login(&hub, &host);
        assert!(first.kick.is_cancelled());
        assert!(first
            .drain()
            .iter()
            .any(|message| matches!(message, ServerMessage::AuthRejected { .. })));
        // The stale socket closing later must not take the new one down.
        hub.disconnect(first.id);
        assert_eq!(hub.connected_count(), 1);
    }

    #[test]
    fn failed_auth_closes_the_connection() {
        let hub = hub();
        let stranger = TestDevice::new("Stranger", Role::Controller);
        let mut socket = open(&hub);
        let _ = hub.challenge(socket.id);
        hub.handle(
            socket.id,
            ClientMessage::Auth {
                identity: Identity {
                    id: stranger.info.id.clone(),
                    public_key: stranger.info.public_key.clone(),
                },
                signature: String::new(),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: "Stranger".into(),
            },
        )
        .unwrap();
        assert!(socket.kick.is_cancelled());
        assert!(matches!(
            socket.drain().as_slice(),
            [ServerMessage::AuthRejected { .. }]
        ));
    }

    #[test]
    fn auth_ok_carries_turn_credentials_when_enabled() {
        let hub = hub().with_turn(Some(TurnConfig {
            server: "10.0.0.2:3478".into(),
            secret: "secret".into(),
            ttl: Duration::from_secs(60),
            relay_bitrate_ceiling: Some(4_000_000),
        }));
        let host = TestDevice::new("Mac", Role::Host);
        hub.store.upsert_device(&host.info).unwrap();
        hub.refresh_devices().unwrap();
        let mut socket = open(&hub);
        let ServerMessage::Challenge {
            nonce, server_id, ..
        } = hub.challenge(socket.id)
        else {
            unreachable!()
        };
        use base64::Engine;
        let signature = host
            .signing
            .sign(&Challenge { nonce, server_id }.signing_payload());
        hub.handle(
            socket.id,
            ClientMessage::Auth {
                identity: Identity {
                    id: host.info.id.clone(),
                    public_key: host.info.public_key.clone(),
                },
                signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: "Mac".into(),
            },
        )
        .unwrap();
        let turn = socket
            .drain()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::AuthOk { turn, .. } => turn,
                _ => None,
            });
        let turn = turn.expect("credentials are minted");
        assert_eq!(turn.server, "10.0.0.2:3478");
        assert_eq!(turn.relay_bitrate_ceiling, Some(4_000_000));
        assert!(!turn.username.is_empty() && !turn.password.is_empty());
    }

    #[test]
    fn rtt_updates_are_rate_limited_by_magnitude() {
        assert!(rtt_changed_meaningfully(None, 20));
        assert!(!rtt_changed_meaningfully(Some(20), 22));
        assert!(rtt_changed_meaningfully(Some(20), 30));
        assert!(!rtt_changed_meaningfully(Some(200), 210));
    }

    #[test]
    fn rtt_is_reported_to_other_peers() {
        let hub = hub();
        let host = TestDevice::new("Mac", Role::Host);
        let alice = TestDevice::new("Alice", Role::Controller);
        let host_socket = login(&hub, &host);
        let mut alice_socket = login(&hub, &alice);
        alice_socket.drain();
        hub.record_rtt(host_socket.id, Duration::from_millis(42));
        let rtt = alice_socket
            .drain()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::PeerUpdate { peers } => peers[0].rtt_ms,
                _ => None,
            });
        assert_eq!(rtt, Some(42));
    }

    #[test]
    fn slow_consumers_are_dropped() {
        let hub = hub();
        let (outbox, _rx) = channel(1);
        let kick = CancellationToken::new();
        let id = hub.connect(Connection {
            outbox,
            kick: kick.clone(),
        });
        hub.handle(id, ClientMessage::Ping { nonce: 1 }).unwrap();
        assert!(!kick.is_cancelled());
        hub.handle(id, ClientMessage::Ping { nonce: 2 }).unwrap();
        assert!(kick.is_cancelled(), "a full outbox kicks the connection");
    }
}
