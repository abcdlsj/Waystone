//! WSS client for the signaling channel, with certificate pinning.
//!
//! Native clients cannot rely on the public CA store because a self-hosted
//! instance uses a self-signed certificate. Instead the certificate is pinned by
//! its SHA-256 fingerprint, which the pairing URI carried.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async_tls_with_config, Connector, WebSocketStream};

use waystone_proto::signal::{ClientMessage, ServerMessage};

type Socket = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

/// A connected and authenticated signaling socket.
pub struct Signal {
    socket: Socket,
}

impl Signal {
    /// Connects to `endpoint` (`host:port`) and pins `fingerprint`.
    ///
    /// Returns the socket plus the challenge the server opened with, so the
    /// caller can authenticate immediately.
    pub async fn connect(endpoint: &str, fingerprint: &str) -> Result<(Signal, ServerMessage)> {
        let address = endpoint
            .to_socket_addrs_first()
            .with_context(|| format!("resolving {endpoint}"))?;
        let stream = TcpStream::connect(address)
            .await
            .with_context(|| format!("connecting to {address}"))?;

        let config = pinned_client_config(fingerprint)?;
        let connector = Connector::Rustls(config);
        // The pin replaces hostname verification, so the SNI name is irrelevant.
        let url = format!("wss://{endpoint}/ws");
        let (socket, _response) = client_async_tls_with_config(
            url,
            stream,
            Some(WebSocketConfig::default()),
            Some(connector),
        )
        .await
        .context("websocket handshake failed (is the certificate fingerprint correct?)")?;

        let mut signal = Signal { socket };
        let challenge = signal.next_message().await?;
        Ok((signal, challenge))
    }

    /// Sends one message.
    pub async fn send(&mut self, message: &ClientMessage) -> Result<()> {
        let text = message.to_json();
        self.socket
            .send(Message::Text(text.into()))
            .await
            .context("sending a signaling message")
    }

    /// Waits for the next server message.
    pub async fn next_message(&mut self) -> Result<ServerMessage> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let message = ServerMessage::from_json(&text)
                        .with_context(|| format!("unexpected server message: {text}"))?;
                    return Ok(message);
                }
                Some(Ok(Message::Close(frame))) => {
                    return Err(anyhow!("server closed the channel: {frame:?}"))
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(anyhow!("signaling socket error: {error}")),
                None => return Err(anyhow!("signaling socket ended")),
            }
        }
    }

}

/// Derives the pinned client configuration.
fn pinned_client_config(fingerprint: &str) -> Result<Arc<ClientConfig>> {
    let expected = fingerprint.to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "fingerprint must be 64 hex characters, got {fingerprint:?}"
        ));
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            expected,
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Accepts exactly one certificate: the one whose SHA-256 matches the pin.
#[derive(Debug)]
struct PinnedVerifier {
    expected: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = Sha256::digest(end_entity.as_ref());
        let actual = hex::encode(digest);
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {}",
                self.expected, actual
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

trait ToSocketAddr {
    fn to_socket_addrs_first(&self) -> Result<std::net::SocketAddr>;
}

impl ToSocketAddr for str {
    fn to_socket_addrs_first(&self) -> Result<std::net::SocketAddr> {
        use std::net::ToSocketAddrs;
        let mut addresses = self.to_socket_addrs()?;
        addresses
            .next()
            .ok_or_else(|| anyhow!("{self} resolved to no address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_malformed_fingerprint() {
        assert!(pinned_client_config("abc").is_err());
        assert!(pinned_client_config(&"z".repeat(64)).is_err());
        assert!(pinned_client_config(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn accepts_uppercase_fingerprints() {
        // The pairing URI renders lowercase, but a hand typed pin may not.
        assert!(pinned_client_config(&"A".repeat(64)).is_ok());
    }
}
