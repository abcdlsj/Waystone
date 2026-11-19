//! End-to-end session test.
//!
//! Two engines run in the same process over the loopback interface. A tiny
//! in-memory relay forwards SDP and ICE candidates between them exactly like the
//! signaling server does. The test then drives one video frame, one Opus packet
//! and one input frame through the real transport.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use waystone_proto::Viewport;
use waystone_rtc::engine::{sink, Engine, EngineConfig, Event, Role, SessionOptions};

#[derive(Debug, Clone)]
enum Signal {
    Offer(String),
    Answer(String),
    Candidate,
}

/// Starts an engine whose events are forwarded into a channel.
async fn start(role: Role, relayed_events: UnboundedSender<Event>) -> Engine {
    let sink = sink(move |event: Event| {
        let _ = relayed_events.send(event);
    });
    let mut config = EngineConfig::new(role, sink).with_bind("127.0.0.1:0".parse().unwrap());
    if role == Role::Controller {
        config = config.with_session(SessionOptions::new(
            Viewport {
                width: 1280,
                height: 720,
                dpi: 300,
                fps: 60,
            },
            true,
        ));
    }
    Engine::start(config).await.expect("engine starts")
}

/// Waits for the next event matching `predicate`.
async fn wait_for<F>(
    events: &mut UnboundedReceiver<Event>,
    what: &str,
    mut predicate: F,
) -> Event
where
    F: FnMut(&Event) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(event)) if predicate(&event) => return event,
            Ok(Some(other)) => { eprintln!("[{what}] skipped: {other:?}"); continue },
            Ok(None) => panic!("event channel closed while waiting for {what}"),
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_negotiates_and_carries_media() {
    let (controller_tx, mut controller_events) = unbounded_channel();
    let (host_tx, mut host_events) = unbounded_channel();

    let controller = start(Role::Controller, controller_tx).await;
    let host = start(Role::Host, host_tx).await;

    controller.offer();

    // Signaling relay: forward offer, answer and candidates verbatim.
    let offer = wait_for(&mut controller_events, "local offer", |event| {
        matches!(event, Event::LocalOffer(_))
    })
    .await;
    let Event::LocalOffer(offer_sdp) = offer else {
        unreachable!()
    };
    host.accept_offer(offer_sdp);

    let answer = wait_for(&mut host_events, "local answer", |event| {
        matches!(event, Event::LocalAnswer(_))
    })
    .await;
    let Event::LocalAnswer(answer_sdp) = answer else {
        unreachable!()
    };
    controller.accept_answer(answer_sdp);

    // Trickle candidates in both directions until both sides are connected.
    let mut controller_connected = false;
    let mut host_connected = false;
    let mut host_media = None;
    while !(controller_connected && host_connected) {
        tokio::select! {
            event = controller_events.recv() => match event {
                Some(Event::LocalCandidate(candidate)) => host.add_remote_candidate(candidate),
                Some(Event::Connected) => controller_connected = true,
                Some(Event::Closed(reason)) => panic!("controller closed: {reason}"),
                Some(_) => {}
                None => panic!("controller channel closed"),
            },
            event = host_events.recv() => match event {
                Some(Event::LocalCandidate(candidate)) => controller.add_remote_candidate(candidate),
                Some(Event::Connected) => host_connected = true,
                Some(media @ Event::MediaReady { .. }) => host_media = Some(media),
                Some(Event::Closed(reason)) => panic!("host closed: {reason}"),
                Some(_) => {}
                None => panic!("host channel closed"),
            },
            _ = tokio::time::sleep(Duration::from_secs(20)) => panic!("connect timeout"),
        }
    }

    let ready = match host_media {
        Some(media) => media,
        None => {
            wait_for(&mut host_events, "host media", |event| {
                matches!(event, Event::MediaReady { video: Some(_), .. })
            })
            .await
        }
    };
    let Event::MediaReady { video, audio } = ready else {
        unreachable!()
    };
    assert!(video.is_some(), "video mid must be negotiated");
    assert!(audio.is_some(), "audio mid must be negotiated");

    // Host sends one keyframe sized payload; the controller must receive it.
    // The bytes are not a real HEVC bitstream, but str0m only has to depacketize
    // them, so a synthetic Annex-B frame exercises the same path.
    // An IDR NAL (type 19) preceded by VPS/SPS/PPS the way VideoToolbox emits it.
    // str0m caches parameter sets and prepends them to the IDR packet.
    let annex_b_keyframe = {
        let mut frame = Vec::new();
        for (nal_type, payload) in [
            (32u8, &[0x0c, 0x01, 0xff][..]), // VPS
            (33u8, &[0x0c, 0x01, 0xff][..]), // SPS
            (34u8, &[0x0c, 0x01, 0xff][..]), // PPS
            (19u8, &[0xaa, 0xbb, 0xcc, 0xdd][..]), // IDR_W_RADL
        ] {
            frame.extend_from_slice(&[0, 0, 0, 1]);
            frame.extend_from_slice(&[nal_type << 1, 0x01]);
            frame.extend_from_slice(payload);
        }
        frame
    };
    // Retry a few times: the first write may race with the host's send stream.
    for index in 0..10 {
        host.send_video(annex_b_keyframe.clone(), index * 3000);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let frame = wait_for(&mut controller_events, "video frame", |event| {
        matches!(event, Event::VideoFrame { .. })
    })
    .await;
    let Event::VideoFrame { data, .. } = frame else {
        unreachable!()
    };
    assert!(!data.is_empty(), "received frame must not be empty");

    // Audio travels in the other direction on the same session.
    host.send_audio(vec![0xfc, 0x01, 0x02, 0x03], 0);
    let audio = wait_for(&mut controller_events, "audio frame", |event| {
        matches!(event, Event::AudioFrame { .. })
    })
    .await;
    let Event::AudioFrame { data, .. } = audio else {
        unreachable!()
    };
    assert_eq!(data.len(), 4);

    controller.shutdown();
    host.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_channel_reaches_the_host() {
    let (controller_tx, mut controller_events) = unbounded_channel();
    let (host_tx, mut host_events) = unbounded_channel();

    let controller = start(Role::Controller, controller_tx).await;
    let host = start(Role::Host, host_tx).await;

    controller.offer();
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

    // Wait for the data channels before writing.
    let mut reliable_open = false;
    let mut fast_open = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !(reliable_open && fast_open) {
        tokio::select! {
            event = controller_events.recv() => match event {
                Some(Event::LocalCandidate(candidate)) => host.add_remote_candidate(candidate),
                Some(Event::ChannelOpen(label)) => {
                    if label == "input-reliable" { reliable_open = true; }
                    if label == "input-fast" { fast_open = true; }
                }
                Some(Event::Closed(reason)) => panic!("controller closed: {reason}"),
                Some(_) => {}
                None => panic!("controller channel closed"),
            },
            event = host_events.recv() => match event {
                Some(Event::LocalCandidate(candidate)) => controller.add_remote_candidate(candidate),
                Some(Event::Closed(reason)) => panic!("host closed: {reason}"),
                Some(_) => {}
                None => panic!("host channel closed"),
            },
            _ = tokio::time::sleep_until(deadline) => panic!("channel open timeout"),
        }
    }

    controller.send_input(true, vec![0x06]);
    let input = wait_for(&mut host_events, "reliable input", |event| {
        matches!(event, Event::Input { reliable: true, .. })
    })
    .await;
    let Event::Input { data, .. } = input else {
        unreachable!()
    };
    assert_eq!(data, vec![0x06]);

    controller.shutdown();
    host.shutdown();
}

/// Compile-time check that the engine handle is cheap to share.
fn _assert_shareable(engine: Engine) {
    let cloned = engine.clone();
    let shared = Arc::new(cloned);
    drop(shared);
}
