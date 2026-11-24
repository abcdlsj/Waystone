//! Proves the relay path end to end.
//!
//! The embedded TURN server runs on loopback, a host and a controller engine
//! both allocate on it in relay-only mode, and the signaling is relayed
//! in-process. Every datagram between the engines therefore crosses two TURN
//! allocations, and the test checks that video and input frames arrive and
//! that both sides report the path as relayed.

use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use waystone_proto::Viewport;
use waystone_rtc::engine::{
    sink, Engine, EngineConfig, Event, Role, SessionOptions, TurnConfig, CHANNEL_INPUT_RELIABLE,
};
use waystoned::turnserver::{self, TurnServer, TurnServerConfig};

const SECRET: &str = "relay-test-secret";

async fn start_turn() -> TurnServer {
    let mut config = TurnServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        "127.0.0.1".parse().unwrap(),
        SECRET,
    );
    // Both engines live on this machine, so their relays talk over loopback.
    config.allow_loopback_peers = true;
    TurnServer::start(config).await.expect("turn server starts")
}

async fn start_engine(role: Role, turn: TurnConfig, events: UnboundedSender<Event>) -> Engine {
    let sink = sink(move |event: Event| {
        let _ = events.send(event);
    });
    let mut config = EngineConfig::new(role, sink)
        .with_bind("127.0.0.1:0".parse().unwrap())
        .with_turn(turn)
        .with_relay_only(true);
    if role == Role::Controller {
        config = config.with_session(SessionOptions::new(
            Viewport {
                width: 1280,
                height: 720,
                dpi: 300,
                fps: 60,
            },
            false,
        ));
    }
    Engine::start(config).await.expect("engine starts")
}

fn turn_config(server: &TurnServer, password: Option<&str>) -> TurnConfig {
    let (username, minted) = turnserver::credentials(SECRET, Duration::from_secs(600));
    TurnConfig {
        server: server.local_addr(),
        username,
        password: password.map(str::to_string).unwrap_or(minted),
        relay_bitrate_ceiling: 4_000_000,
    }
}

/// What one side has seen so far.
#[derive(Default)]
struct Seen {
    connected: bool,
    reliable_open: bool,
    relayed_path: bool,
    direct_path: bool,
    video: usize,
    input: Vec<Vec<u8>>,
    candidates: Vec<String>,
}

impl Seen {
    fn record(&mut self, side: &str, event: &Event, peer: &Engine) {
        match event {
            Event::LocalCandidate(candidate) => {
                self.candidates.push(candidate.clone());
                peer.add_remote_candidate(candidate.clone());
            }
            Event::Connected => self.connected = true,
            Event::ChannelOpen(label) if label == CHANNEL_INPUT_RELIABLE => {
                self.reliable_open = true
            }
            Event::PathUpdate { relayed: true, .. } => self.relayed_path = true,
            Event::PathUpdate { relayed: false, .. } => self.direct_path = true,
            Event::VideoFrame { .. } => self.video += 1,
            Event::Input { data, .. } => self.input.push(data.clone()),
            Event::Closed(reason) => panic!("{side} closed: {reason}"),
            _ => {}
        }
    }
}

struct Session {
    host: Engine,
    controller: Engine,
    host_events: UnboundedReceiver<Event>,
    controller_events: UnboundedReceiver<Event>,
    host_seen: Seen,
    controller_seen: Seen,
}

impl Session {
    /// Pumps events (forwarding candidates) until `done` holds.
    async fn pump_until(&mut self, what: &str, done: impl Fn(&Seen, &Seen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(&self.host_seen, &self.controller_seen) {
            tokio::select! {
                Some(event) = self.host_events.recv() => {
                    self.host_seen.record("host", &event, &self.controller);
                }
                Some(event) = self.controller_events.recv() => {
                    self.controller_seen.record("controller", &event, &self.host);
                }
                _ = tokio::time::sleep_until(deadline) => panic!("timed out waiting for {what}"),
            }
        }
    }
}

/// A synthetic HEVC keyframe: VPS, SPS, PPS and an IDR slice in Annex-B form.
fn keyframe() -> Vec<u8> {
    let mut frame = Vec::new();
    for (nal_type, payload) in [
        (32u8, &[0x0c, 0x01, 0xff][..]),
        (33u8, &[0x0c, 0x01, 0xff][..]),
        (34u8, &[0x0c, 0x01, 0xff][..]),
        (19u8, &[0xaa, 0xbb, 0xcc, 0xdd][..]),
    ] {
        frame.extend_from_slice(&[0, 0, 0, 1]);
        frame.extend_from_slice(&[nal_type << 1, 0x01]);
        frame.extend_from_slice(payload);
    }
    frame
}

async fn wait_for(
    events: &mut UnboundedReceiver<Event>,
    what: &str,
    predicate: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) if predicate(&event) => return event,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_and_input_flow_through_the_relay() {
    let turn = start_turn().await;

    let (host_tx, mut host_events) = unbounded_channel();
    let (controller_tx, mut controller_events) = unbounded_channel();
    let host = start_engine(Role::Host, turn_config(&turn, None), host_tx).await;
    let controller = start_engine(Role::Controller, turn_config(&turn, None), controller_tx).await;

    // The controller offers on its own; relay offer and answer verbatim.
    let Event::LocalOffer(offer) = wait_for(&mut controller_events, "offer", |event| {
        matches!(event, Event::LocalOffer(_))
    })
    .await
    else {
        unreachable!()
    };
    host.accept_offer(offer);
    let Event::LocalAnswer(answer) = wait_for(&mut host_events, "answer", |event| {
        matches!(event, Event::LocalAnswer(_))
    })
    .await
    else {
        unreachable!()
    };
    controller.accept_answer(answer);

    let mut session = Session {
        host,
        controller,
        host_events,
        controller_events,
        host_seen: Seen::default(),
        controller_seen: Seen::default(),
    };

    session
        .pump_until("connection and data channels", |host, controller| {
            host.connected && controller.connected && controller.reliable_open
        })
        .await;

    // Video: host to controller. Retry until one frame makes it through.
    let frame = keyframe();
    let mut rtp_time = 0u64;
    let deadline = Instant::now() + Duration::from_secs(15);
    while session.controller_seen.video == 0 {
        assert!(
            Instant::now() < deadline,
            "no video frame crossed the relay"
        );
        session.host.send_video(frame.clone(), rtp_time);
        rtp_time += 3000;
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until && session.controller_seen.video == 0 {
            tokio::select! {
                Some(event) = session.host_events.recv() => {
                    session.host_seen.record("host", &event, &session.controller);
                }
                Some(event) = session.controller_events.recv() => {
                    session.controller_seen.record("controller", &event, &session.host);
                }
                _ = tokio::time::sleep_until(until) => {}
            }
        }
    }

    // Input: controller to host on the reliable channel.
    session.controller.send_input(true, vec![0x06, 0x01]);
    session
        .pump_until("reliable input on the host", |host, _| {
            host.input.iter().any(|data| data == &[0x06, 0x01])
        })
        .await;

    // Both sides must report the selected pair as relayed, and never direct.
    session
        .pump_until("relayed path updates", |host, controller| {
            host.relayed_path && controller.relayed_path
        })
        .await;
    assert!(
        !session.host_seen.direct_path,
        "host reported a direct path"
    );
    assert!(
        !session.controller_seen.direct_path,
        "controller reported a direct path"
    );

    // Relay-only means nothing but relay candidates was ever advertised.
    for candidate in session
        .host_seen
        .candidates
        .iter()
        .chain(&session.controller_seen.candidates)
    {
        assert!(
            candidate.contains("typ relay"),
            "non-relay candidate advertised: {candidate}"
        );
    }
    assert!(!session.host_seen.candidates.is_empty());
    assert!(!session.controller_seen.candidates.is_empty());

    session.controller.shutdown();
    session.host.shutdown();
    turn.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_credentials_close_a_relay_only_session() {
    let turn = start_turn().await;
    let (tx, mut events) = unbounded_channel();
    let _engine = start_engine(Role::Host, turn_config(&turn, Some("not-the-password")), tx).await;
    let closed = wait_for(&mut events, "close", |event| {
        matches!(event, Event::Closed(_))
    })
    .await;
    let Event::Closed(reason) = closed else {
        unreachable!()
    };
    assert!(reason.contains("401"), "unexpected reason: {reason}");
    turn.close().await;
}
