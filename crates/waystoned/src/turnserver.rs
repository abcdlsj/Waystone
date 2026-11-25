//! Embedded STUN/TURN server.
//!
//! Runs on `UDP 3478` and answers both STUN binding requests (used by clients to
//! discover their public address) and TURN allocations (used as the relayed
//! fallback path). Credentials are derived from a shared secret, so the daemon
//! never stores per-client TURN passwords.
//!
//! Relays never talk to loopback, unspecified, link-local, multicast or
//! broadcast addresses: a TURN server is an open UDP forwarder for anyone with
//! credentials, and those ranges would expose services on the server itself.
//! Private ranges stay reachable because a self-hosted server usually relays
//! for peers on its own LAN.

use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use webrtc_util::Conn;

use turn::auth::{generate_auth_key, AuthHandler};
use turn::relay::RelayAddressGenerator;
use turn::server::config::{ConnConfig, ServerConfig};
use turn::server::Server;

/// Realm advertised by the built-in server. Fixed so credentials stay valid
/// across restarts.
pub const REALM: &str = "waystone";

/// Longest username accepted; real ones are `expiry:waystone`.
const MAX_USERNAME: usize = 128;

/// Options for [`TurnServer::start`].
#[derive(Debug, Clone)]
pub struct TurnServerConfig {
    /// UDP address to listen on.
    pub listen: SocketAddr,
    /// Address advertised inside relayed candidates; for a NAT'ed host it must
    /// be the public address, not the listen address.
    pub public_ip: IpAddr,
    /// Shared secret the credentials are derived from.
    pub secret: String,
    /// Lets relays reach loopback peers. Only for tests that run every party
    /// on one machine; never enable it on a reachable server.
    pub allow_loopback_peers: bool,
    /// Ports relayed allocations bind, so a cloud firewall can open exactly
    /// these; `None` lets the OS pick any ephemeral port.
    pub relay_ports: Option<RangeInclusive<u16>>,
}

impl TurnServerConfig {
    pub fn new(listen: SocketAddr, public_ip: IpAddr, secret: impl Into<String>) -> Self {
        TurnServerConfig {
            listen,
            public_ip,
            secret: secret.into(),
            allow_loopback_peers: false,
            relay_ports: None,
        }
    }
}

/// A running TURN server.
pub struct TurnServer {
    server: Server,
    local_addr: SocketAddr,
}

impl TurnServer {
    /// Binds the listener and starts serving.
    pub async fn start(config: TurnServerConfig) -> Result<TurnServer> {
        let socket = UdpSocket::bind(config.listen)
            .await
            .with_context(|| format!("binding TURN listener {}", config.listen))?;
        let local_addr = socket.local_addr()?;
        info!(
            "turn listening on {local_addr}, advertising {}",
            config.public_ip
        );

        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn: Arc::new(socket),
                relay_addr_generator: Box::new(FilteredRelay {
                    relay_address: config.public_ip,
                    allow_loopback: config.allow_loopback_peers,
                    ports: config.relay_ports.clone(),
                    next_port: AtomicU16::new(0),
                }),
            }],
            realm: REALM.to_owned(),
            auth_handler: Arc::new(SecretAuth::new(&config.secret)),
            channel_bind_timeout: Duration::from_secs(600),
            alloc_close_notify: None,
        })
        .await
        .context("starting the TURN server")?;

        Ok(TurnServer { server, local_addr })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops the server and releases every allocation.
    pub async fn close(self) {
        self.server.close().await.ok();
    }
}

/// Runs the TURN server until `cancel` fires.
///
/// `public_ip` is the address advertised inside relayed candidates; for a NAT'ed
/// host it must be the public address, not the listen address.
pub async fn run(
    listen: SocketAddr,
    public_ip: &str,
    secret: &str,
    relay_ports: Option<RangeInclusive<u16>>,
    cancel: CancellationToken,
) -> Result<()> {
    let relay_ip = IpAddr::from_str(public_ip).unwrap_or_else(|_| {
        warn!("{public_ip} is not an IP address, using 127.0.0.1 for relayed candidates");
        IpAddr::from([127, 0, 0, 1])
    });
    let mut config = TurnServerConfig::new(listen, relay_ip, secret);
    config.relay_ports = relay_ports;
    let server = TurnServer::start(config).await?;
    cancel.cancelled().await;
    info!("turn shutting down");
    server.close().await;
    Ok(())
}

/// True when a relay may exchange datagrams with `ip`.
fn peer_allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    if ip.is_loopback() {
        return allow_loopback;
    }
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !(v4.is_link_local() || v4.is_broadcast() || v4.octets()[0] == 0),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// Allocates relay sockets that refuse forbidden peers.
///
/// turn 0.17 has no hook to reject a permission for a peer address, so the
/// check sits on the relay socket itself: nothing is sent to, or accepted
/// from, a forbidden address.
struct FilteredRelay {
    relay_address: IpAddr,
    allow_loopback: bool,
    ports: Option<RangeInclusive<u16>>,
    /// Offset of the next port to try, so freed ports are not reused at once.
    next_port: AtomicU16,
}

impl FilteredRelay {
    /// Binds a relay socket: the requested port, else the first free port of
    /// the configured range, else any port.
    async fn bind(&self, ip: IpAddr, requested_port: u16) -> std::io::Result<UdpSocket> {
        let range = match &self.ports {
            Some(range) if requested_port == 0 => range.clone(),
            _ => return UdpSocket::bind(SocketAddr::new(ip, requested_port)).await,
        };
        let size = u32::from(*range.end()) - u32::from(*range.start()) + 1;
        let first = u32::from(self.next_port.fetch_add(1, Ordering::Relaxed)) % size;
        for step in 0..size {
            let port = *range.start() as u32 + (first + step) % size;
            match UdpSocket::bind(SocketAddr::new(ip, port as u16)).await {
                Ok(socket) => return Ok(socket),
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("every relay port in {range:?} is taken"),
        ))
    }
}

#[async_trait]
impl RelayAddressGenerator for FilteredRelay {
    fn validate(&self) -> Result<(), turn::Error> {
        Ok(())
    }

    fn supports_family(&self, use_ipv4: bool) -> bool {
        self.relay_address.is_ipv4() == use_ipv4
    }

    async fn allocate_conn(
        &self,
        use_ipv4: bool,
        requested_port: u16,
    ) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if !self.supports_family(use_ipv4) {
            return Err(turn::Error::ErrInvalidRequestedFamilyValue);
        }
        let ip = if use_ipv4 {
            IpAddr::from([0, 0, 0, 0])
        } else {
            IpAddr::from([0u16; 8])
        };
        let socket = self
            .bind(ip, requested_port)
            .await
            .map_err(|error| turn::Error::Other(error.to_string()))?;
        let mut relay_addr = socket
            .local_addr()
            .map_err(|error| turn::Error::Other(error.to_string()))?;
        relay_addr.set_ip(self.relay_address);
        let conn = FilteredConn {
            socket,
            allow_loopback: self.allow_loopback,
        };
        Ok((Arc::new(conn), relay_addr))
    }
}

struct FilteredConn {
    socket: UdpSocket,
    allow_loopback: bool,
}

#[async_trait]
impl Conn for FilteredConn {
    async fn connect(&self, addr: SocketAddr) -> webrtc_util::Result<()> {
        if !peer_allowed(addr.ip(), self.allow_loopback) {
            return Err(forbidden(addr));
        }
        Ok(self.socket.connect(addr).await?)
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        Ok(self.recv_from(buf).await?.0)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        loop {
            let (len, from) = self.socket.recv_from(buf).await?;
            if peer_allowed(from.ip(), self.allow_loopback) {
                return Ok((len, from));
            }
            debug!("relay dropped a datagram from forbidden peer {from}");
        }
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        Ok(self.socket.send(buf).await?)
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> webrtc_util::Result<usize> {
        if !peer_allowed(target.ip(), self.allow_loopback) {
            debug!("relay refused to send to forbidden peer {target}");
            return Err(forbidden(target));
        }
        Ok(self.socket.send_to(buf, target).await?)
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn forbidden(peer: SocketAddr) -> webrtc_util::Error {
    webrtc_util::Error::Other(format!("relaying to {peer} is not allowed"))
}

/// Long term credentials derived from a shared secret.
///
/// Username format is `expiry:name`, where `expiry` is a Unix timestamp; the
/// password is `base64(HMAC-SHA1(secret, username))`. This is the scheme coturn
/// uses for `use-auth-secret`, which keeps the daemon free of per-user state.
///
/// Nothing is cached: deriving a key is two hashes, and turn 0.17 already keeps
/// the key of each live allocation, so refreshes keep working after expiry.
struct SecretAuth {
    secret: String,
}

impl SecretAuth {
    fn new(secret: &str) -> Self {
        SecretAuth {
            secret: secret.to_string(),
        }
    }

    fn key_for(&self, username: &str, realm: &str) -> Vec<u8> {
        generate_auth_key(username, realm, &password_for(&self.secret, username))
    }
}

/// True when the expiry timestamp at the start of `username` is in the future.
fn username_valid(username: &str, now: u64) -> bool {
    if username.len() > MAX_USERNAME {
        return false;
    }
    let expiry = username.split(':').next().unwrap_or_default();
    matches!(expiry.parse::<u64>(), Ok(expiry) if expiry > now)
}

impl AuthHandler for SecretAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        if !username_valid(username, unix_now()) {
            debug!("rejecting expired or malformed TURN username from {src_addr}");
            return Err(turn::Error::Other("credentials expired".into()));
        }
        Ok(self.key_for(username, realm))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn password_for(secret: &str, username: &str) -> String {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(username.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Builds a username/password pair clients can use, coturn `use-auth-secret` style.
pub fn credentials(secret: &str, ttl: Duration) -> (String, String) {
    let expiry = unix_now() + ttl.as_secs();
    let username = format!("{expiry}:waystone");
    let password = password_for(secret, &username);
    (username, password)
}

/// Kept so the daemon can log the credential lifetime it hands out.
pub const CREDENTIAL_TTL: Duration = Duration::from_secs(3600);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_derived_from_the_secret() {
        let (user, password) = credentials("secret", Duration::from_secs(60));
        assert!(user.contains(":waystone"));
        assert!(!password.is_empty());
        // Different secrets produce different passwords for the same user.
        let (_, other) = credentials("other", Duration::from_secs(60));
        assert_ne!(password, other);
    }

    #[test]
    fn auth_key_matches_the_turn_expectation() {
        let auth = SecretAuth::new("secret");
        let first = auth.key_for("user", REALM);
        let second = auth.key_for("user", REALM);
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn expired_and_malformed_usernames_are_rejected() {
        let auth = SecretAuth::new("secret");
        let source: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let (valid, _) = credentials("secret", Duration::from_secs(60));
        assert!(auth.auth_handle(&valid, REALM, source).is_ok());
        assert!(auth.auth_handle("1:waystone", REALM, source).is_err());
        assert!(auth.auth_handle("waystone", REALM, source).is_err());
        let long = format!("{}:{}", u64::MAX, "x".repeat(200));
        assert!(auth.auth_handle(&long, REALM, source).is_err());
    }

    #[test]
    fn forbidden_peers() {
        let allowed = |ip: &str| peer_allowed(ip.parse().unwrap(), false);
        assert!(!allowed("127.0.0.1"));
        assert!(!allowed("::1"));
        assert!(!allowed("::ffff:127.0.0.1"));
        assert!(!allowed("0.0.0.0"));
        assert!(!allowed("::"));
        assert!(!allowed("224.0.0.1"));
        assert!(!allowed("255.255.255.255"));
        assert!(!allowed("169.254.1.1"));
        assert!(!allowed("fe80::1"));
        assert!(allowed("192.168.1.10"));
        assert!(allowed("203.0.113.5"));
        assert!(allowed("2001:db8::1"));
        assert!(peer_allowed("127.0.0.1".parse().unwrap(), true));
    }

    #[tokio::test]
    async fn relays_bind_inside_the_configured_range() {
        let taken = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let start = taken.local_addr().unwrap().port();
        let relay = FilteredRelay {
            relay_address: IpAddr::from([203, 0, 113, 7]),
            allow_loopback: false,
            ports: Some(start..=start.saturating_add(1)),
            next_port: AtomicU16::new(0),
        };
        // The first port is taken, so the range moves on to the second.
        let (_held, addr) = relay.allocate_conn(true, 0).await.unwrap();
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 7], start + 1)));
        // Both are taken now: no silent fallback to a port outside the range.
        assert!(relay.allocate_conn(true, 0).await.is_err());
    }

    #[tokio::test]
    async fn bind_failure_is_reported() {
        // Port 0 always binds, so bind a socket first and reuse its address to
        // force the failure path.
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let result = run(
            address,
            "127.0.0.1",
            "secret",
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err(), "binding a taken port must fail");
    }
}
