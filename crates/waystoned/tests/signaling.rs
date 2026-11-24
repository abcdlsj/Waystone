//! End-to-end test of the HTTPS/WSS surface.
//!
//! A real server is started on a random port and driven over TLS with a pinned
//! certificate, exactly like the phone does: pair with a token, authenticate with
//! an Ed25519 signature, then open a session between two devices.

// The port lock is held across awaits on purpose: it serializes the tests that
// bind a real TCP port.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{client_async_tls_with_config, Connector};

use waystone_proto::device::{DeviceId, DeviceInfo, Platform, Role, Viewport};
use waystone_proto::identity::{encode_public_key, Challenge, Identity};
use waystone_proto::pairing::PairingUri;
use waystone_proto::signal::{ClientMessage, ServerMessage};

use waystoned::http::{router, AppState};
use waystoned::hub::Hub;
use waystoned::instance::Instance;
use waystoned::store::Store;

/// Serializes the integration tests.
///
/// Every test starts a real TLS server on a probe-selected port; running them in
/// parallel makes the port selection racy, so they take turns instead.
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A running test server plus the material needed to talk to it.
struct TestServer {
    addr: SocketAddr,
    fingerprint: String,
    store: Arc<Store>,
    endpoint: String,
    turn: String,
    _dir: std::path::PathBuf,
}

impl TestServer {
    async fn start() -> TestServer {
        // Server and test client share one crypto provider.
        waystoned::tls::install_crypto_provider();
        let dir = std::env::temp_dir().join(format!(
            "waystone-it-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let instance =
            Instance::load_or_create(&dir, &["localhost".to_string(), "127.0.0.1".to_string()])
                .unwrap();

        let store = Arc::new(Store::open(Store::default_path(&dir)).unwrap());
        {
            let mut writer = Store::open(Store::default_path(&dir)).unwrap();
            writer
                .ensure_instance(&instance.server_id, &instance.fingerprint)
                .unwrap();
        }

        // Reserve a port, then hand it to the server.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let endpoint = format!("127.0.0.1:{}", addr.port());
        let turn = String::from("127.0.0.1:3478");
        let state = AppState {
            store: store.clone(),
            hub: Hub::new(instance.server_id.clone(), store.clone()),
            fingerprint: instance.fingerprint.clone(),
            endpoint: endpoint.clone(),
            turn_endpoint: turn.clone(),
            admin_password_hash: None,
        };

        let tls = waystoned::tls::config(&instance).await.unwrap();
        let app = router(state);
        let server = axum_server::bind_rustls(addr, tls)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>());
        tokio::spawn(async move {
            let _ = server.await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        TestServer {
            addr,
            fingerprint: instance.fingerprint.clone(),
            store,
            endpoint,
            turn,
            _dir: dir,
        }
    }

    fn client_config(&self) -> Arc<ClientConfig> {
        // Pin the instance certificate like the app does.
        let pem = std::fs::read_to_string(self._dir.join("server.crt")).unwrap();
        let mut roots = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    async fn connect(
        &self,
    ) -> (
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
            tokio_tungstenite::tungstenite::Message,
        >,
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        >,
    ) {
        let stream = TcpStream::connect(self.addr).await.unwrap();
        let connector = Connector::Rustls(self.client_config());
        let (socket, _response) = client_async_tls_with_config(
            format!("wss://127.0.0.1:{}/ws", self.addr.port()),
            stream,
            Some(WebSocketConfig::default()),
            Some(connector),
        )
        .await
        .expect("websocket handshake");
        let _ = ServerName::try_from("localhost").unwrap();
        socket.split()
    }

    fn pairing_uri(&self, role: Role) -> PairingUri {
        waystoned::pairing::create(
            &self.store,
            &self.endpoint,
            &self.turn,
            &self.fingerprint,
            role,
        )
        .unwrap()
    }
}

/// One enrolled device, with its key material.
struct Client {
    signing: SigningKey,
    info: DeviceInfo,
}

impl Client {
    fn new(name: &str, platform: Platform) -> Client {
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let public = signing.verifying_key().to_bytes();
        let info = DeviceInfo {
            id: DeviceId::from_public_key(&public),
            name: name.to_string(),
            platform,
            role: Role::Controller,
            public_key: encode_public_key(&public),
        };
        Client { signing, info }
    }

    fn identity(&self) -> Identity {
        Identity {
            id: self.info.id.clone(),
            public_key: self.info.public_key.clone(),
        }
    }

    fn sign(&self, challenge: &Challenge) -> String {
        use base64::Engine;
        let signature = self.signing.sign(&challenge.signing_payload());
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }
}

async fn next_message<S>(stream: &mut S) -> ServerMessage
where
    S: StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for a server message"
        );
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                return ServerMessage::from_json(&text).expect("server message parses")
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(error))) => panic!("socket error: {error}"),
            Ok(None) => panic!("socket closed"),
            Err(_) => panic!("timed out waiting for a server message"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_auth_and_session_flow() {
    let _guard = test_lock();
    let server = TestServer::start().await;

    // --- host enrollment -----------------------------------------------------
    let host = Client::new("Mac Studio", Platform::Macos);
    let uri = server.pairing_uri(Role::Host);
    let (mut host_tx, mut host_rx) = server.connect().await;
    let challenge = match next_message(&mut host_rx).await {
        ServerMessage::Challenge {
            nonce, server_id, ..
        } => Challenge { nonce, server_id },
        other => panic!("expected a challenge, got {other:?}"),
    };
    let mut host_info = host.info.clone();
    host_info.role = Role::Controller; // the token decides the real role
    host_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::PairRequest {
                token: uri.token.clone(),
                device: host_info,
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    match next_message(&mut host_rx).await {
        ServerMessage::PairAccepted { .. } => {}
        other => panic!("expected PairAccepted, got {other:?}"),
    }

    // Reconnecting issues a fresh challenge; the first one is consumed.
    let _ = challenge;
    let (mut host_tx2, mut host_rx2) = server.connect().await;
    let challenge = match next_message(&mut host_rx2).await {
        ServerMessage::Challenge {
            nonce, server_id, ..
        } => Challenge { nonce, server_id },
        other => panic!("expected a challenge, got {other:?}"),
    };
    host_tx2
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::Auth {
                identity: host.identity(),
                signature: host.sign(&challenge),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: "Mac Studio".into(),
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    match next_message(&mut host_rx2).await {
        ServerMessage::AuthOk { .. } => {}
        other => panic!("expected AuthOk, got {other:?}"),
    }

    // --- controller enrollment ----------------------------------------------
    let controller = Client::new("iPhone", Platform::Ios);
    let uri = server.pairing_uri(Role::Controller);
    let (mut controller_tx, mut controller_rx) = server.connect().await;
    let _ = next_message(&mut controller_rx).await;
    controller_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::PairRequest {
                token: uri.token.clone(),
                device: controller.info.clone(),
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        next_message(&mut controller_rx).await,
        ServerMessage::PairAccepted { .. }
    ));

    let (mut controller_tx, mut controller_rx) = server.connect().await;
    let challenge = match next_message(&mut controller_rx).await {
        ServerMessage::Challenge {
            nonce, server_id, ..
        } => Challenge { nonce, server_id },
        other => panic!("expected a challenge, got {other:?}"),
    };
    controller_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::Auth {
                identity: controller.identity(),
                signature: controller.sign(&challenge),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: "iPhone".into(),
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();

    // The controller sees the host in its peer list.
    let peers = loop {
        match next_message(&mut controller_rx).await {
            ServerMessage::AuthOk { peers, .. } => break peers,
            ServerMessage::PeerUpdate { .. } => continue,
            other => panic!("expected AuthOk, got {other:?}"),
        }
    };
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].device.id, host.info.id);
    assert!(peers[0].online, "the host is connected");
    assert_eq!(peers[0].device.role, Role::Host);

    // --- session --------------------------------------------------------------
    controller_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::SessionStart {
                target: host.info.id.clone(),
                viewport: Viewport {
                    width: 2556,
                    height: 1179,
                    dpi: 460,
                    fps: 60,
                },
                audio: true,
                video_codec: waystone_proto::VideoCodec::H265,
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();

    let session_id = loop {
        match next_message(&mut controller_rx).await {
            // A peer update may be queued between the two messages.
            ServerMessage::PeerUpdate { .. } => continue,
            ServerMessage::SessionAccepted {
                session_id,
                is_host,
                ..
            } => {
                assert!(!is_host);
                break session_id;
            }
            other => panic!("expected SessionAccepted, got {other:?}"),
        }
    };
    loop {
        match next_message(&mut host_rx2).await {
            ServerMessage::PeerUpdate { .. } => continue,
            ServerMessage::SessionAccepted {
                session_id: id,
                is_host,
                ..
            } => {
                assert!(is_host);
                assert_eq!(id, session_id);
                break;
            }
            other => panic!("expected SessionAccepted on the host, got {other:?}"),
        }
    }

    // An SDP offer travels to the host, and the answer comes back.
    controller_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::RtcSignal {
                session_id: session_id.clone(),
                signal: waystone_proto::signal::RtcSignal::Offer {
                    sdp: "v=0\r\n".into(),
                },
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    match next_message(&mut host_rx2).await {
        ServerMessage::RtcSignal { signal, .. } => match signal {
            waystone_proto::signal::RtcSignal::Offer { sdp } => assert_eq!(sdp, "v=0\r\n"),
            other => panic!("expected an offer, got {other:?}"),
        },
        other => panic!("expected RtcSignal, got {other:?}"),
    }

    // Hanging up notifies both ends with the reason.
    controller_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::SessionBye {
                session_id: session_id.clone(),
                reason: waystone_proto::signal::ByeReason::LockAndExit,
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
    for message in [
        next_message(&mut controller_rx).await,
        next_message(&mut host_rx2).await,
    ] {
        match message {
            ServerMessage::SessionEnded { reason, .. } => {
                assert_eq!(reason, waystone_proto::signal::ByeReason::LockAndExit);
            }
            other => panic!("expected SessionEnded, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_a_forged_signature() {
    let _guard = test_lock();
    let server = TestServer::start().await;
    let client = Client::new("Attacker", Platform::Ios);
    let uri = server.pairing_uri(Role::Controller);

    let (mut tx, mut rx) = server.connect().await;
    let _ = next_message(&mut rx).await;
    tx.send(tokio_tungstenite::tungstenite::Message::Text(
        ClientMessage::PairRequest {
            token: uri.token.clone(),
            device: client.info.clone(),
        }
        .to_json()
        .into(),
    ))
    .await
    .unwrap();
    assert!(matches!(
        next_message(&mut rx).await,
        ServerMessage::PairAccepted { .. }
    ));

    let (mut tx, mut rx) = server.connect().await;
    let challenge = match next_message(&mut rx).await {
        ServerMessage::Challenge {
            nonce, server_id, ..
        } => Challenge { nonce, server_id },
        other => panic!("expected a challenge, got {other:?}"),
    };
    // Sign with a different key than the one that was paired.
    let impostor = SigningKey::from_bytes(&[7u8; 32]);
    use base64::Engine;
    let signature = impostor.sign(&challenge.signing_payload());
    tx.send(tokio_tungstenite::tungstenite::Message::Text(
        ClientMessage::Auth {
            identity: client.identity(),
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            protocol_version: waystone_proto::PROTOCOL_VERSION,
            name: "Attacker".into(),
        }
        .to_json()
        .into(),
    ))
    .await
    .unwrap();

    match next_message(&mut rx).await {
        ServerMessage::AuthRejected { reason } => {
            assert!(reason.contains("signature"), "unexpected reason: {reason}")
        }
        other => panic!("expected AuthRejected, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_token_is_single_use_over_the_wire() {
    let _guard = test_lock();
    let server = TestServer::start().await;
    let uri = server.pairing_uri(Role::Controller);

    for attempt in 0..2 {
        let client = Client::new(&format!("phone-{attempt}"), Platform::Ios);
        let (mut tx, mut rx) = server.connect().await;
        let _ = next_message(&mut rx).await;
        tx.send(tokio_tungstenite::tungstenite::Message::Text(
            ClientMessage::PairRequest {
                token: uri.token.clone(),
                device: client.info,
            }
            .to_json()
            .into(),
        ))
        .await
        .unwrap();
        let message = next_message(&mut rx).await;
        if attempt == 0 {
            assert!(matches!(message, ServerMessage::PairAccepted { .. }));
        } else {
            assert!(matches!(message, ServerMessage::PairRejected { .. }));
        }
    }
}
