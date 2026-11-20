//! Signaling hub: authenticates devices and routes messages between them.
//!
//! The hub owns no sockets. It keeps a registry of connected devices and hands
//! out a per-connection outbox, so the WebSocket handler stays trivial and the
//! routing rules can be unit tested without any network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use waystone_proto::device::{DeviceId, DeviceInfo, Role};
use waystone_proto::identity::Identity;
use waystone_proto::signal::{
    version_compatible, ByeReason, ClientMessage, PeerState, ServerMessage,
};

use crate::challenge::ChallengeStore;
use crate::store::Store;

/// Messages pushed to one connected device.
pub type Outbox = UnboundedSender<ServerMessage>;

/// One authenticated device.
#[derive(Clone)]
struct Peer {
    connection: u64,
    info: DeviceInfo,
    outbox: Outbox,
    session: Option<String>,
}

/// A live session between a host and a controller.
#[derive(Clone)]
struct Session {
    host: DeviceId,
    controller: DeviceId,
}

/// Shared state of the signaling layer.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    store: Arc<Store>,
    challenges: Arc<ChallengeStore>,
}

struct HubInner {
    server_id: String,
    /// Relay credentials handed to every authenticated device.
    turn: Option<waystone_proto::signal::TurnCredentials>,
    /// Outboxes of connections that have not authenticated yet, keyed by
    /// connection id so pairing and auth rejections can still be delivered.
    pending: HashMap<u64, Outbox>,
    peers: HashMap<DeviceId, Peer>,
    sessions: HashMap<String, Session>,
}

impl Hub {
    /// Creates a hub for `server_id`, backed by `store`.
    pub fn new(server_id: impl Into<String>, store: Arc<Store>) -> Self {
        Hub {
            inner: Arc::new(Mutex::new(HubInner {
                server_id: server_id.into(),
                turn: None,
                pending: HashMap::new(),
                peers: HashMap::new(),
                sessions: HashMap::new(),
            })),
            store,
            challenges: Arc::new(ChallengeStore::new()),
        }
    }

    /// Registers a new connection with the outbox that the socket handler drains
    /// into the WebSocket.
    ///
    /// The first message the caller sends is the challenge from [`Hub::challenge`].
    pub fn connect(&self, outbox: Outbox) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let connection_id = self.challenges.next_connection();
        inner.pending.insert(connection_id, outbox);
        connection_id
    }

    /// Sets the relay credentials handed out after authentication.
    pub fn set_turn_credentials(&self, turn: Option<waystone_proto::signal::TurnCredentials>) {
        self.inner.lock().unwrap().turn = turn;
    }

    /// Issues the per connection challenge.
    ///
    /// Signatures are only valid for one connection, so an attacker who records a
    /// handshake cannot replay it.
    pub fn challenge(&self, connection_id: u64) -> ServerMessage {
        let server_id = self.inner.lock().unwrap().server_id.clone();
        let challenge = self.challenges.issue(connection_id, &server_id);
        ServerMessage::Challenge {
            nonce: challenge.nonce,
            server_id: challenge.server_id,
            protocol_version: waystone_proto::PROTOCOL_VERSION,
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
            } => self.handle_auth(connection_id, identity, &signature, protocol_version, &name),
            ClientMessage::SessionStart {
                target,
                viewport,
                audio,
                video_codec,
            } => self.handle_session_start(connection_id, target, viewport, audio, video_codec),
            ClientMessage::RtcSignal { session_id, signal } => {
                let message = ServerMessage::RtcSignal {
                    session_id: session_id.clone(),
                    signal,
                };
                self.relay(connection_id, &session_id, message)
            }
            ClientMessage::SessionControl {
                session_id,
                control,
            } => {
                let message = ServerMessage::SessionControl {
                    session_id: session_id.clone(),
                    control,
                };
                self.relay(connection_id, &session_id, message)
            }
            ClientMessage::SessionBye { session_id, reason } => {
                self.handle_bye(connection_id, &session_id, reason)
            }
            ClientMessage::Ping { nonce } => {
                self.send_to_connection(connection_id, ServerMessage::Pong { nonce });
                Ok(())
            }
        }
    }

    /// Removes a connection and tears down the session it participated in.
    pub fn disconnect(&self, connection_id: u64) {
        self.challenges.forget(connection_id);
        let mut inner = self.inner.lock().unwrap();
        inner.pending.remove(&connection_id);

        let device = inner
            .peers
            .iter()
            .find(|(_, peer)| peer.connection == connection_id)
            .map(|(id, _)| id.clone());
        let Some(device) = device else {
            return;
        };
        let Some(peer) = inner.peers.remove(&device) else {
            return;
        };
        info!("device {} disconnected", device);

        if let Some(session_id) = peer.session {
            inner.sessions.remove(&session_id);
            self.end_session_locked(&mut inner, &session_id, ByeReason::NetworkTimeout);
        }
        self.broadcast_peer_update(&mut inner);
    }

    /// Peers the given device may see.
    pub fn peer_list(&self, viewer: &DeviceId) -> Vec<PeerState> {
        let mut inner = self.inner.lock().unwrap();
        self.peer_list_locked(&mut inner, viewer)
    }

    /// Number of connected devices, for the admin panel.
    pub fn connected_count(&self) -> usize {
        self.inner.lock().unwrap().peers.len()
    }

    fn handle_pair(&self, connection_id: u64, token: &str, mut device: DeviceInfo) -> Result<()> {
        if device.public_key.is_empty() || device.name.is_empty() {
            return self.reject_pair(connection_id, "device description is incomplete");
        }

        // The id is derived from the key, so a device can never claim an identity
        // it does not own.
        let identity = Identity {
            id: device.id.clone(),
            public_key: device.public_key.clone(),
        };
        if !identity.validate() {
            return self.reject_pair(connection_id, "device id does not match its public key");
        }

        match self.store.consume_pairing_token(token)? {
            Some(role) => {
                // The role comes from the token, not from the device: a phone must
                // not be able to enroll itself as a host.
                device.role = role;
                self.store.upsert_device(&device)?;
                info!("paired {} ({}) as {}", device.name, device.id, role);
                let server_id = self.inner.lock().unwrap().server_id.clone();
                self.send_to_connection(connection_id, ServerMessage::PairAccepted { server_id });
                Ok(())
            }
            None => self.reject_pair(
                connection_id,
                "pairing token is invalid, expired or already used",
            ),
        }
    }

    fn reject_pair(&self, connection_id: u64, reason: &str) -> Result<()> {
        self.send_to_connection(
            connection_id,
            ServerMessage::PairRejected {
                reason: reason.to_string(),
            },
        );
        Ok(())
    }

    fn handle_auth(
        &self,
        connection_id: u64,
        identity: Identity,
        signature: &str,
        protocol_version: u32,
        name: &str,
    ) -> Result<()> {
        if !version_compatible(protocol_version) {
            return self.reject_auth(
                connection_id,
                format!("protocol version {protocol_version} is not supported"),
            );
        }

        let Some(challenge) = self.challenges.take(connection_id) else {
            return self.reject_auth(
                connection_id,
                "no challenge was issued for this connection".to_string(),
            );
        };

        let Some(stored) = self.store.identity(&identity.id)? else {
            return self.reject_auth(
                connection_id,
                "device is not paired with this instance".to_string(),
            );
        };
        if stored.public_key != identity.public_key {
            return self.reject_auth(
                connection_id,
                "public key does not match the paired device".to_string(),
            );
        }

        let Ok(signature_bytes) = base64_decode(signature) else {
            return self.reject_auth(connection_id, "signature is not valid base64".to_string());
        };

        if !crate::challenge::verify(
            &stored.public_key,
            &challenge.signing_payload(),
            &signature_bytes,
        ) {
            warn!("rejected bad signature from {}", identity.id);
            return self.reject_auth(connection_id, "signature verification failed".to_string());
        }

        let Some(stored_device) = self.store.device(&identity.id)? else {
            return self.reject_auth(connection_id, "paired device vanished".to_string());
        };

        let mut inner = self.inner.lock().unwrap();
        let Some(outbox) = inner.pending.remove(&connection_id) else {
            return Err(anyhow!("connection {connection_id} has no outbox"));
        };

        let mut info = stored_device.info;
        if !name.is_empty() && name != info.name {
            info.name = name.to_string();
            let _ = self.store.upsert_device(&info);
        }
        let _ = self.store.touch_device(&info.id);

        let previous = inner.peers.insert(
            info.id.clone(),
            Peer {
                connection: connection_id,
                info: info.clone(),
                outbox: outbox.clone(),
                session: None,
            },
        );
        if let Some(previous) = previous {
            // Kick the stale socket: the new connection wins.
            let _ = previous.outbox.send(ServerMessage::AuthRejected {
                reason: "replaced by a newer connection from the same device".into(),
            });
            if let Some(session_id) = previous.session {
                inner.sessions.remove(&session_id);
                self.end_session_locked(&mut inner, &session_id, ByeReason::NetworkTimeout);
            }
        }

        let peers = self.peer_list_locked(&mut inner, &info.id);
        let turn = inner.turn.clone();
        let _ = outbox.send(ServerMessage::AuthOk { peers, turn });
        self.broadcast_peer_update(&mut inner);
        Ok(())
    }

    fn reject_auth(&self, connection_id: u64, reason: String) -> Result<()> {
        self.send_to_connection(connection_id, ServerMessage::AuthRejected { reason });
        Ok(())
    }

    fn handle_session_start(
        &self,
        connection_id: u64,
        target: DeviceId,
        viewport: waystone_proto::device::Viewport,
        audio: bool,
        video_codec: Option<String>,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(controller) = self.peer_by_connection(&inner, connection_id).cloned() else {
            return Ok(());
        };

        if controller.info.role != Role::Controller {
            self.send_to_connection_locked(
                &mut inner,
                connection_id,
                ServerMessage::SessionRejected {
                    reason: "only a controller can start a session".into(),
                },
            );
            return Ok(());
        }

        let Some(host) = inner.peers.get(&target).cloned() else {
            self.send_to_connection_locked(
                &mut inner,
                connection_id,
                ServerMessage::SessionRejected {
                    reason: "the host is offline".into(),
                },
            );
            return Ok(());
        };
        if host.info.role != Role::Host {
            self.send_to_connection_locked(
                &mut inner,
                connection_id,
                ServerMessage::SessionRejected {
                    reason: "the target device is not a host".into(),
                },
            );
            return Ok(());
        }

        let mut raw = [0u8; 8];
        rand::Rng::fill(&mut rand::rng(), &mut raw);
        let session_id = hex::encode(raw);

        inner.sessions.insert(
            session_id.clone(),
            Session {
                host: target.clone(),
                controller: controller.info.id.clone(),
            },
        );
        if let Some(peer) = inner.peers.get_mut(&controller.info.id) {
            peer.session = Some(session_id.clone());
        }
        if let Some(peer) = inner.peers.get_mut(&target) {
            peer.session = Some(session_id.clone());
        }

        let _ = controller.outbox.send(ServerMessage::SessionAccepted {
            session_id: session_id.clone(),
            is_host: false,
            peer: host.info.clone(),
            viewport: Some(viewport),
            audio,
            video_codec: video_codec.clone(),
        });
        let _ = host.outbox.send(ServerMessage::SessionAccepted {
            session_id,
            is_host: true,
            peer: controller.info.clone(),
            viewport: Some(viewport),
            audio,
            video_codec,
        });
        Ok(())
    }

    fn handle_bye(&self, connection_id: u64, session_id: &str, reason: ByeReason) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.sessions.remove(session_id).is_none() {
            return Ok(());
        }
        self.end_session_locked(&mut inner, session_id, reason);
        let _ = connection_id;
        Ok(())
    }

    /// Notifies both ends that `session_id` ended and clears the peer state.
    fn end_session_locked(&self, inner: &mut HubInner, session_id: &str, reason: ByeReason) {
        let targets: Vec<(DeviceId, Outbox)> = inner
            .peers
            .iter()
            .filter(|(_, peer)| peer.session.as_deref() == Some(session_id))
            .map(|(id, peer)| (id.clone(), peer.outbox.clone()))
            .collect();
        for (id, outbox) in targets {
            if let Some(peer) = inner.peers.get_mut(&id) {
                peer.session = None;
            }
            let _ = outbox.send(ServerMessage::SessionEnded {
                session_id: session_id.to_string(),
                reason,
                detail: None,
            });
        }
    }

    fn relay(&self, connection_id: u64, session_id: &str, message: ServerMessage) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let Some(session) = inner.sessions.get(session_id) else {
            return Ok(());
        };
        let Some(sender) = self.peer_by_connection(&inner, connection_id) else {
            return Ok(());
        };
        let target = if sender.info.id == session.host {
            session.controller.clone()
        } else if sender.info.id == session.controller {
            session.host.clone()
        } else {
            // Not a participant: silently drop instead of leaking session state.
            return Ok(());
        };
        if let Some(peer) = inner.peers.get(&target) {
            let _ = peer.outbox.send(message);
        }
        Ok(())
    }

    fn broadcast_peer_update(&self, inner: &mut HubInner) {
        let ids: Vec<DeviceId> = inner.peers.keys().cloned().collect();
        for id in ids {
            let peers = self.peer_list_locked(inner, &id);
            if let Some(peer) = inner.peers.get(&id) {
                let _ = peer.outbox.send(ServerMessage::PeerUpdate { peers });
            }
        }
    }

    fn peer_list_locked(&self, inner: &mut HubInner, viewer: &DeviceId) -> Vec<PeerState> {
        let mut list: Vec<PeerState> = self
            .store
            .devices()
            .unwrap_or_default()
            .into_iter()
            .filter(|stored| &stored.info.id != viewer)
            .map(|stored| PeerState {
                online: inner.peers.contains_key(&stored.info.id),
                device: stored.info,
                rtt_ms: None,
                hostname: None,
            })
            .collect();
        // Hosts first, then by name: the phone list must not jump around.
        list.sort_by(|a, b| {
            let rank = |role: Role| if role == Role::Host { 0 } else { 1 };
            rank(a.device.role)
                .cmp(&rank(b.device.role))
                .then_with(|| a.device.name.cmp(&b.device.name))
        });
        list
    }

    fn peer_by_connection<'a>(&self, inner: &'a HubInner, connection_id: u64) -> Option<&'a Peer> {
        inner
            .peers
            .values()
            .find(|peer| peer.connection == connection_id)
    }

    fn send_to_connection(&self, connection_id: u64, message: ServerMessage) {
        let mut inner = self.inner.lock().unwrap();
        self.send_to_connection_locked(&mut inner, connection_id, message);
    }

    fn send_to_connection_locked(
        &self,
        inner: &mut HubInner,
        connection_id: u64,
        message: ServerMessage,
    ) {
        if let Some(outbox) = inner.pending.get(&connection_id) {
            let _ = outbox.send(message);
            return;
        }
        if let Some(peer) = inner
            .peers
            .values()
            .find(|peer| peer.connection == connection_id)
        {
            let _ = peer.outbox.send(message);
        }
    }
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| anyhow!("{error}"))
}

/// Turnaround after which a peer is considered stale by the admin panel.
pub const PEER_STALE_AFTER: Duration = Duration::from_secs(60);
