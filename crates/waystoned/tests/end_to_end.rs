//! End-to-end test: server, host and controller in one process.
//!
//! This is the closest thing to the real product that runs without screen
//! recording permission: signaling goes through the actual HTTPS/WSS server and
//! the media through the actual WebRTC engine on both ends.
//!
//! Chain under test:
//!
//! ```text
//! pair(host) -> auth(host) -> pair(controller) -> auth(controller)
//!   -> session_start -> SDP offer/answer + trickle ICE over the socket
//!   -> HEVC frame host -> controller
//!   -> input frame controller -> host
//!   -> session_bye -> both sides torn down
//! ```

// The port lock is held across awaits on purpose: it serializes the tests that
// bind a real TCP port.
#![allow(clippy::await_holding_lock)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async_tls_with_config, Connector};

use waystone_proto::device::{DeviceId, DeviceInfo, Platform, Role, Viewport};
use waystone_proto::identity::{encode_public_key, Challenge, Identity};
use waystone_proto::signal::{ByeReason, ClientMessage, RtcSignal, ServerMessage};
use waystone_rtc::engine::{sink, Engine, EngineConfig, Event, SessionOptions};

use waystoned::http::{router, AppState};
use waystoned::hub::Hub;
use waystoned::instance::Instance;
use waystoned::store::Store;

/// Serializes the integration tests: each one binds a real TLS port.
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ============================== test server ==============================

struct TestServer {
    addr: SocketAddr,
    store: Arc<Store>,
    dir: std::path::PathBuf,
}

impl TestServer {
    async fn start() -> TestServer {
        waystoned::tls::install_crypto_provider();

        let dir = std::env::temp_dir().join(format!(
            "waystone-e2e-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let instance =
            Instance::load_or_create(&dir, &["localhost".to_string(), "127.0.0.1".to_string()])
                .unwrap();

        let path = Store::default_path(&dir);
        let store = Arc::new(Store::open(&path).unwrap());
        Store::open(&path)
            .unwrap()
            .ensure_instance(&instance.server_id, &instance.fingerprint)
            .unwrap();

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let state = AppState {
            store: store.clone(),
            hub: Hub::new(instance.server_id.clone(), store.clone()),
            fingerprint: instance.fingerprint.clone(),
            endpoint: format!("127.0.0.1:{}", addr.port()),
            turn_endpoint: "127.0.0.1:3478".into(),
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

        TestServer { addr, store, dir }
    }

    fn pairing_token(&self, role: Role) -> String {
        self.store
            .create_pairing_token(role, Duration::from_secs(600))
            .unwrap()
            .token
    }

    fn client_config(&self) -> Arc<tokio_rustls::rustls::ClientConfig> {
        use tokio_rustls::rustls::{ClientConfig, RootCertStore};
        let pem = std::fs::read_to_string(self.dir.join("server.crt")).unwrap();
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

    /// Connects a device and enrolls it, returning the authenticated handle.
    async fn enroll(&self, name: &str, platform: Platform, role: Role) -> Device {
        let mut device = Device::new(name, platform);
        let (sender, receiver) = self.connect().await;
        device.outbox = sender;
        device.inbox = receiver;

        let challenge = device
            .expect("challenge", |message| {
                matches!(message, ServerMessage::Challenge { .. })
            })
            .await;
        let ServerMessage::Challenge {
            nonce, server_id, ..
        } = challenge
        else {
            unreachable!()
        };

        device
            .send(ClientMessage::PairRequest {
                token: self.pairing_token(role),
                device: device.info.clone(),
            })
            .await;
        device
            .expect("pair accepted", |message| {
                matches!(message, ServerMessage::PairAccepted { .. })
            })
            .await;

        // The same connection authenticates; the challenge is still unused
        // because pairing does not consume it.
        device
            .send(ClientMessage::Auth {
                identity: device.identity(),
                signature: device.sign(&Challenge { nonce, server_id }),
                protocol_version: waystone_proto::PROTOCOL_VERSION,
                name: device.info.name.clone(),
            })
            .await;
        device
            .expect("auth ok", |message| {
                matches!(message, ServerMessage::AuthOk { .. })
            })
            .await;
        device
    }

    /// Opens a socket and spawns the two pumps that copy frames in both
    /// directions.
    async fn connect(
        &self,
    ) -> (
        UnboundedSender<ClientMessage>,
        UnboundedReceiver<ServerMessage>,
    ) {
        let stream = TcpStream::connect(self.addr).await.unwrap();
        let connector = Connector::Rustls(self.client_config());
        let (socket, _) = client_async_tls_with_config(
            format!("wss://127.0.0.1:{}/ws", self.addr.port()),
            stream,
            Some(WebSocketConfig::default()),
            Some(connector),
        )
        .await
        .expect("websocket handshake");

        let (mut writer, mut reader) = socket.split();
        let (tx, mut rx) = unbounded_channel::<ClientMessage>();
        let (event_tx, event_rx) = unbounded_channel::<ServerMessage>();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if writer
                    .send(Message::Text(message.to_json().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(Ok(Message::Text(text))) = reader.next().await {
                if let Ok(message) = ServerMessage::from_json(&text) {
                    if event_tx.send(message).is_err() {
                        break;
                    }
                }
            }
        });

        (tx, event_rx)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

// ============================== test device ==============================

struct Device {
    signing: SigningKey,
    info: DeviceInfo,
    outbox: UnboundedSender<ClientMessage>,
    inbox: UnboundedReceiver<ServerMessage>,
}

impl Device {
    fn new(name: &str, platform: Platform) -> Device {
        let signing = SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let public = signing.verifying_key().to_bytes();
        Device {
            signing,
            info: DeviceInfo {
                id: DeviceId::from_public_key(&public),
                name: name.to_string(),
                platform,
                role: Role::Controller,
                public_key: encode_public_key(&public),
            },
            outbox: unbounded_channel().0,
            inbox: unbounded_channel().1,
        }
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

    async fn send(&self, message: ClientMessage) {
        let _ = self.outbox.send(message);
    }

    /// Sends an SDP/ICE message for `session`.
    async fn signal(&self, session: &str, signal: RtcSignal) {
        self.send(ClientMessage::RtcSignal {
            session_id: session.to_string(),
            signal,
        })
        .await;
    }

    async fn expect<F>(&mut self, what: &str, mut predicate: F) -> ServerMessage
    where
        F: FnMut(&ServerMessage) -> bool,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for {what}");
            match tokio::time::timeout(remaining, self.inbox.recv()).await {
                Ok(Some(message)) if predicate(&message) => return message,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("socket closed while waiting for {what}"),
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }
}

// ============================== media engines ==============================

async fn start_engine(
    role: waystone_rtc::Role,
    viewport: Option<Viewport>,
) -> (Engine, UnboundedReceiver<Event>) {
    let (tx, rx) = unbounded_channel();
    let mut config = EngineConfig::new(
        role,
        sink(move |event| {
            let _ = tx.send(event);
        }),
    )
    .with_bind("127.0.0.1:0".parse().unwrap());
    if let Some(viewport) = viewport {
        config = config.with_session(SessionOptions::new(viewport, true));
    }
    let engine = Engine::start(config).await.expect("engine starts");
    (engine, rx)
}

async fn wait_event<F>(events: &mut UnboundedReceiver<Event>, what: &str, mut predicate: F) -> Event
where
    F: FnMut(&Event) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(event)) if predicate(&event) => return event,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("engine closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

/// A synthetic HEVC keyframe: VPS, SPS, PPS and an IDR NAL.
fn synthetic_keyframe() -> Vec<u8> {
    let mut frame = Vec::new();
    for (nal_type, payload) in [
        (32u8, &[0x0c, 0x01, 0xff][..]),
        (33u8, &[0x0c, 0x01, 0xff][..]),
        (34u8, &[0x0c, 0x01, 0xff][..]),
        (19u8, &[0xaa, 0xbb, 0xcc, 0xdd, 0xee][..]),
    ] {
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(&[nal_type << 1, 0x01]);
        frame.extend_from_slice(payload);
    }
    frame
}

/// Moves engine events that must reach the peer onto the socket, and tracks the
/// connection state.
struct Pump {
    connected: bool,
    video_frames: usize,
}

impl Pump {
    fn new() -> Pump {
        Pump {
            connected: false,
            video_frames: 0,
        }
    }

    /// Drains everything the engine produced so far.
    async fn drain(
        &mut self,
        engine: &Engine,
        events: &mut UnboundedReceiver<Event>,
        device: &Device,
        session: &str,
    ) {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::LocalOffer(sdp) => {
                    device.signal(session, RtcSignal::Offer { sdp }).await;
                }
                Event::LocalAnswer(sdp) => {
                    device.signal(session, RtcSignal::Answer { sdp }).await;
                }
                Event::LocalCandidate(candidate) => {
                    device
                        .signal(
                            session,
                            RtcSignal::Ice {
                                candidate,
                                sdp_mid: None,
                                sdp_mline_index: None,
                            },
                        )
                        .await;
                }
                Event::Connected => self.connected = true,
                Event::VideoFrame { .. } => self.video_frames += 1,
                Event::Closed(reason) => panic!("engine closed: {reason}"),
                _ => {}
            }
            let _ = engine;
        }
    }
}

/// Applies a relayed signal to an engine.
fn apply(engine: &Engine, signal: RtcSignal) {
    match signal {
        RtcSignal::Offer { sdp } => engine.accept_offer(sdp),
        RtcSignal::Answer { sdp } => engine.accept_answer(sdp),
        RtcSignal::Ice { candidate, .. } => engine.add_remote_candidate(candidate),
        RtcSignal::IceComplete => {}
    }
}

// ================================== test ==================================

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn full_session_delivers_media_both_ways() {
    let _guard = test_lock();
    let server = TestServer::start().await;

    let mut host = server
        .enroll("Mac Studio", Platform::Macos, Role::Host)
        .await;
    let mut controller = server
        .enroll("iPhone", Platform::Ios, Role::Controller)
        .await;
    let host_id = host.info.id.clone();
    let controller_id = controller.info.id.clone();

    let (host_engine, mut host_events) = start_engine(waystone_rtc::Role::Host, None).await;
    let viewport = Viewport {
        width: 1280,
        height: 720,
        dpi: 300,
        fps: 60,
    };
    let (controller_engine, mut controller_events) =
        start_engine(waystone_rtc::Role::Controller, Some(viewport)).await;

    // The controller offers as soon as its engine is up.
    let offer = wait_event(&mut controller_events, "offer", |event| {
        matches!(event, Event::LocalOffer(_))
    })
    .await;
    let Event::LocalOffer(offer_sdp) = offer else {
        unreachable!()
    };

    controller
        .send(ClientMessage::SessionStart {
            target: host_id,
            viewport,
            audio: true,
        })
        .await;
    let session_id = match controller
        .expect("session accepted", |message| {
            matches!(message, ServerMessage::SessionAccepted { .. })
        })
        .await
    {
        ServerMessage::SessionAccepted { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    host.expect("session accepted on the host", |message| {
        matches!(message, ServerMessage::SessionAccepted { .. })
    })
    .await;

    controller
        .signal(&session_id, RtcSignal::Offer { sdp: offer_sdp })
        .await;

    // --- signaling pump until both engines are connected ---------------------
    let mut host_pump = Pump::new();
    let mut controller_pump = Pump::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while !(host_pump.connected && controller_pump.connected) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out establishing the media path"
        );
        tokio::select! {
            message = host.inbox.recv() => match message {
                Some(ServerMessage::RtcSignal { signal, .. }) => apply(&host_engine, signal),
                Some(ServerMessage::SessionEnded { reason, .. }) => {
                    panic!("host session ended early: {reason:?}")
                }
                Some(_) => {}
                None => panic!("host socket closed"),
            },
            message = controller.inbox.recv() => match message {
                Some(ServerMessage::RtcSignal { signal, .. }) => apply(&controller_engine, signal),
                Some(ServerMessage::SessionEnded { reason, .. }) => {
                    panic!("controller session ended early: {reason:?}")
                }
                Some(_) => {}
                None => panic!("controller socket closed"),
            },
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        host_pump
            .drain(&host_engine, &mut host_events, &host, &session_id)
            .await;
        controller_pump
            .drain(
                &controller_engine,
                &mut controller_events,
                &controller,
                &session_id,
            )
            .await;
    }

    // --- media host -> controller -------------------------------------------
    // Frames are counted inside the pump, so draining the channel never loses
    // the very frame the assertion is about.
    let frame = synthetic_keyframe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut sequence = 0u64;
    while controller_pump.video_frames == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a video frame"
        );
        host_engine.send_video(frame.clone(), sequence * 3000);
        sequence += 1;
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Keep the signaling flowing: ICE consent checks need it.
        while let Ok(message) = host.inbox.try_recv() {
            if let ServerMessage::RtcSignal { signal, .. } = message {
                apply(&host_engine, signal);
            }
        }
        while let Ok(message) = controller.inbox.try_recv() {
            if let ServerMessage::RtcSignal { signal, .. } = message {
                apply(&controller_engine, signal);
            }
        }
        host_pump
            .drain(&host_engine, &mut host_events, &host, &session_id)
            .await;
        controller_pump
            .drain(
                &controller_engine,
                &mut controller_events,
                &controller,
                &session_id,
            )
            .await;
    }
    assert!(
        controller_pump.video_frames > 0,
        "the controller must receive at least one frame"
    );

    // --- input controller -> host -------------------------------------------
    controller_engine.send_input(true, vec![0x06]);
    let input = wait_event(&mut host_events, "input frame", |event| {
        matches!(event, Event::Input { .. })
    })
    .await;
    let Event::Input { data, .. } = input else {
        unreachable!()
    };
    assert_eq!(data, vec![0x06]);

    // --- teardown -----------------------------------------------------------
    controller
        .send(ClientMessage::SessionBye {
            session_id: session_id.clone(),
            reason: ByeReason::LockAndExit,
        })
        .await;
    for message in [
        controller
            .expect("controller session ended", |message| {
                matches!(message, ServerMessage::SessionEnded { .. })
            })
            .await,
        host.expect("host session ended", |message| {
            matches!(message, ServerMessage::SessionEnded { .. })
        })
        .await,
    ] {
        match message {
            ServerMessage::SessionEnded { reason, .. } => {
                assert_eq!(reason, ByeReason::LockAndExit)
            }
            _ => unreachable!(),
        }
    }

    let devices = server.store.devices().unwrap();
    assert_eq!(devices.len(), 2, "both devices stay in the pool");
    assert!(devices.iter().any(|device| device.info.id == controller_id));

    controller_engine.shutdown();
    host_engine.shutdown();
}
