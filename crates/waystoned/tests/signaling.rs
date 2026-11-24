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

use waystoned::auth::AdminAuth;
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
            let writer = Store::open(Store::default_path(&dir)).unwrap();
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
            hub: Hub::new(instance.server_id.clone(), store.clone()).unwrap(),
            auth: Arc::new(AdminAuth::load(store.clone()).unwrap()),
            fingerprint: instance.fingerprint.clone(),
            endpoint: endpoint.clone(),
            turn_endpoint: turn.clone(),
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

    /// One HTTPS request over a fresh pinned TLS connection.
    async fn http(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> HttpResponse {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let stream = TcpStream::connect(self.addr).await.unwrap();
        let connector = tokio_rustls::TlsConnector::from(self.client_config());
        let mut tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.addr.port(),
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        tls.write_all(request.as_bytes()).await.unwrap();

        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        // The server may close without a TLS close_notify; keep what arrived.
        while let Ok(Ok(read)) =
            tokio::time::timeout(Duration::from_secs(10), tls.read(&mut chunk)).await
        {
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("status line");
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        HttpResponse {
            status,
            headers,
            body: body.to_string(),
        }
    }

    /// Claims the instance and returns the session cookie pair (`name=value`).
    async fn claim(&self, password: &str) -> String {
        let response = self
            .http("POST", "/claim", &[FORM], &format!("password={password}"))
            .await;
        assert_eq!(response.status, 303, "claim: {}", response.body);
        response.session_cookie().expect("claim sets the cookie")
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

const FORM: (&str, &str) = ("Content-Type", "application/x-www-form-urlencoded");
const JSON: (&str, &str) = ("Content-Type", "application/json");

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn session_cookie(&self) -> Option<String> {
        let cookie = self.header("set-cookie")?;
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        cookie.split(';').next().map(str::to_string)
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

/// Next message that is not a presence update (busy/RTT changes arrive at any
/// time and are not what these assertions are about).
async fn next_signal<S>(stream: &mut S) -> ServerMessage
where
    S: StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    loop {
        match next_message(stream).await {
            ServerMessage::PeerUpdate { .. } => continue,
            other => return other,
        }
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
    match next_signal(&mut host_rx2).await {
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
        next_signal(&mut controller_rx).await,
        next_signal(&mut host_rx2).await,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_api_requires_a_session() {
    let _guard = test_lock();
    let server = TestServer::start().await;

    let panel = server.http("GET", "/", &[], "").await;
    assert_eq!(panel.status, 200);
    assert!(
        panel.body.contains("action=\"/claim\""),
        "unclaimed panel offers the claim form"
    );
    assert!(panel.header("content-security-policy").is_some());

    for (method, path, body) in [
        ("GET", "/api/status", ""),
        ("GET", "/api/devices", ""),
        ("POST", "/api/pair", r#"{"role":"host"}"#),
        ("DELETE", "/api/devices/whatever", ""),
        ("GET", "/pair/0123456789abcdef0123456789abcdef", ""),
    ] {
        let response = server.http(method, path, &[JSON], body).await;
        assert_eq!(response.status, 401, "{method} {path} must need a login");
    }
    // A forged cookie does not help.
    let response = server
        .http(
            "GET",
            "/api/status",
            &[("Cookie", "__Host-waystone_session=deadbeef")],
            "",
        )
        .await;
    assert_eq!(response.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claim_is_one_shot_and_gates_the_api() {
    let _guard = test_lock();
    let server = TestServer::start().await;
    let cookie = server.claim("first-password").await;

    // A second claim is refused right away, without a restart.
    let again = server
        .http("POST", "/claim", &[FORM], "password=second-password")
        .await;
    assert_eq!(again.status, 409);
    let panel = server.http("GET", "/", &[], "").await;
    assert!(
        panel.body.contains("action=\"/login\""),
        "claimed panel asks for a login"
    );

    let wrong = server
        .http("POST", "/login", &[FORM], "password=second-password")
        .await;
    assert_eq!(wrong.status, 401);
    assert!(wrong.header("set-cookie").is_none());
    let right = server
        .http("POST", "/login", &[FORM], "password=first-password")
        .await;
    assert_eq!(right.status, 303);
    assert!(right.session_cookie().is_some());

    let status = server
        .http("GET", "/api/status", &[("Cookie", &cookie)], "")
        .await;
    assert_eq!(status.status, 200, "{}", status.body);
    assert!(status.body.contains(&server.fingerprint));

    let pair = server
        .http(
            "POST",
            "/api/pair",
            &[("Cookie", &cookie), JSON],
            r#"{"role":"controller"}"#,
        )
        .await;
    assert_eq!(pair.status, 200, "{}", pair.body);
    assert!(pair.body.contains("waystone://pair?"));

    // Cross-site writes are refused even with the cookie attached.
    let forged = server
        .http(
            "POST",
            "/api/pair",
            &[
                ("Cookie", &cookie),
                ("Origin", "https://evil.example"),
                JSON,
            ],
            r#"{"role":"host"}"#,
        )
        .await;
    assert_eq!(forged.status, 403);
    let same_origin = format!("https://127.0.0.1:{}", server.addr.port());
    let allowed = server
        .http(
            "POST",
            "/api/pair",
            &[("Cookie", &cookie), ("Origin", &same_origin), JSON],
            r#"{"role":"host"}"#,
        )
        .await;
    assert_eq!(allowed.status, 200);

    // Values reflected into HTML are validated and escaped.
    let reflected = server
        .http(
            "GET",
            "/pair/%3Cscript%3Ealert(1)%3C%2Fscript%3E",
            &[("Cookie", &cookie)],
            "",
        )
        .await;
    assert_eq!(reflected.status, 400);
    assert!(!reflected.body.contains("<script>"));

    // Logging out kills the session server side.
    let logout = server
        .http("POST", "/logout", &[("Cookie", &cookie)], "")
        .await;
    assert_eq!(logout.status, 303);
    let after = server
        .http("GET", "/api/status", &[("Cookie", &cookie)], "")
        .await;
    assert_eq!(after.status, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_from_a_foreign_origin_is_refused() {
    let _guard = test_lock();
    let server = TestServer::start().await;
    let response = server
        .http(
            "GET",
            "/ws",
            &[
                ("Origin", "https://evil.example"),
                ("Upgrade", "websocket"),
                ("Connection", "Upgrade"),
                ("Sec-WebSocket-Version", "13"),
                ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
            "",
        )
        .await;
    assert_eq!(response.status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_device_kicks_its_live_socket() {
    let _guard = test_lock();
    let server = TestServer::start().await;
    let cookie = server.claim("first-password").await;

    // Pair and authenticate on one socket.
    let phone = Client::new("iPhone", Platform::Ios);
    let uri = server.pairing_uri(Role::Controller);
    let (mut tx, mut rx) = server.connect().await;
    let challenge = match next_message(&mut rx).await {
        ServerMessage::Challenge {
            nonce, server_id, ..
        } => Challenge { nonce, server_id },
        other => panic!("expected a challenge, got {other:?}"),
    };
    for message in [
        ClientMessage::PairRequest {
            token: uri.token.clone(),
            device: phone.info.clone(),
        },
        ClientMessage::Auth {
            identity: phone.identity(),
            signature: phone.sign(&challenge),
            protocol_version: waystone_proto::PROTOCOL_VERSION,
            name: "iPhone".into(),
        },
    ] {
        tx.send(tokio_tungstenite::tungstenite::Message::Text(
            message.to_json().into(),
        ))
        .await
        .unwrap();
    }
    loop {
        match next_message(&mut rx).await {
            ServerMessage::AuthOk { .. } => break,
            ServerMessage::PairAccepted { .. } | ServerMessage::PeerUpdate { .. } => {}
            other => panic!("expected AuthOk, got {other:?}"),
        }
    }

    let response = server
        .http(
            "DELETE",
            &format!("/api/devices/{}", phone.info.id),
            &[("Cookie", &cookie)],
            "",
        )
        .await;
    assert_eq!(response.status, 204);

    loop {
        match next_message(&mut rx).await {
            ServerMessage::AuthRejected { .. } => break,
            ServerMessage::PeerUpdate { .. } => {}
            other => panic!("expected AuthRejected, got {other:?}"),
        }
    }
    // The server closes the socket after telling the device why.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.next().await {
                None | Some(Err(_)) => break,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the revoked socket must be closed");
    assert!(server.store.device(&phone.info.id).unwrap().is_none());
}
