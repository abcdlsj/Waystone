//! Embedded STUN/TURN server.
//!
//! Runs on `UDP 3478` and answers both STUN binding requests (used by clients to
//! discover their public address) and TURN allocations (used as the relayed
//! fallback path). Credentials are derived from a shared secret, so the daemon
//! never stores per-client TURN passwords.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use webrtc_util::vnet::net::Net;

use turn::auth::{generate_auth_key, AuthHandler};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::config::{ConnConfig, ServerConfig};
use turn::server::Server;

/// Realm advertised by the built-in server. Fixed so credentials stay valid
/// across restarts.
pub const REALM: &str = "waystone";

/// Runs the TURN server until `cancel` fires.
///
/// `public_ip` is the address advertised inside relayed candidates; for a NAT'ed
/// host it must be the public address, not the listen address.
pub async fn run(
    listen: SocketAddr,
    public_ip: &str,
    secret: &str,
    cancel: CancellationToken,
) -> Result<()> {
    let socket = UdpSocket::bind(listen)
        .await
        .with_context(|| format!("binding TURN listener {listen}"))?;
    info!("turn listening on {listen}, advertising {public_ip}");

    let auth = Arc::new(SecretAuth::new(secret));
    let relay_ip = IpAddr::from_str(public_ip).unwrap_or_else(|_| {
        warn!("{public_ip} is not an IP address, using 127.0.0.1 for relayed candidates");
        IpAddr::from([127, 0, 0, 1])
    });

    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn: Arc::new(socket),
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: relay_ip,
                address: "0.0.0.0".to_owned(),
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: REALM.to_owned(),
        auth_handler: auth,
        channel_bind_timeout: Duration::from_secs(600),
        alloc_close_notify: None,
    })
    .await
    .context("starting the TURN server")?;

    tokio::select! {
        _ = cancel.cancelled() => {
            info!("turn shutting down");
            server.close().await.ok();
        }
    }
    Ok(())
}

/// Long term credentials derived from a shared secret.
///
/// Username format is `expiry:name`; the password is `HMAC(secret, name)`. This is
/// the same scheme coturn uses for `use-auth-secret`, which keeps the daemon free
/// of per-user state.
struct SecretAuth {
    keys: Mutex<HashMap<String, Vec<u8>>>,
    secret: String,
}

impl SecretAuth {
    fn new(secret: &str) -> Self {
        SecretAuth {
            keys: Mutex::new(HashMap::new()),
            secret: secret.to_string(),
        }
    }

    fn key_for(&self, username: &str, realm: &str) -> Vec<u8> {
        // Coturn compatible: password = base64(HMAC-SHA1(secret, username)).
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        let mut mac =
            <Hmac<Sha1> as Mac>::new_from_slice(self.secret.as_bytes()).expect("any key length");
        mac.update(username.as_bytes());
        use base64::Engine;
        let password = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        generate_auth_key(username, realm, &password)
    }
}

impl AuthHandler for SecretAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        // Cache because the TURN server asks for the key on every message.
        let mut keys = self.keys.blocking_lock();
        if let Some(key) = keys.get(username) {
            return Ok(key.clone());
        }
        let key = self.key_for(username, realm);
        keys.insert(username.to_string(), key.clone());
        Ok(key)
    }
}

/// Builds a username/password pair clients can use, coturn `use-auth-secret` style.
pub fn credentials(secret: &str, ttl: Duration) -> (String, String) {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl.as_secs();
    let username = format!("{expiry}:waystone");
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(username.as_bytes());
    let password = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, password)
}

/// Kept so the daemon can log the credential lifetime it hands out.
pub const CREDENTIAL_TTL: Duration = Duration::from_secs(3600);

/// Channel used by tests to observe allocation events.
pub type AllocationSender = mpsc::UnboundedSender<SocketAddr>;

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

    #[tokio::test]
    async fn bind_failure_is_reported() {
        // Port 0 always binds, so bind a socket first and reuse its address to
        // force the failure path.
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let result = run(address, "127.0.0.1", "secret", CancellationToken::new()).await;
        assert!(result.is_err(), "binding a taken port must fail");
    }
}
