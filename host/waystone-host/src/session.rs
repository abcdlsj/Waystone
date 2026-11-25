//! One stream to one controller, from `SessionAccepted` to rollback.
//!
//! A [`Session`] owns everything a stream touches: the WebRTC engine, the
//! capture session, the input injector and the speaker mute. Ending it is
//! [`Session::finish`], which undoes all of it in a fixed order (design doc
//! 4.5): release held input, stop capture, close the transport, restore the
//! speakers, then lock the screen if asked to.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use waystone_proto::signal::{ByeReason, RtcSignal, SessionControl, TurnCredentials};
use waystone_proto::{VideoCodec, Viewport};
use waystone_rtc::engine::{sink, Engine, EngineConfig, Event, Role, TurnConfig};

use crate::capture::FrameSink;
use crate::display::Display;
use crate::input::Injector;
use crate::plan::{plan, StreamPlan};
use crate::platform::{self, Capture, SpeakerMute};

/// How long the media path may stay down before the session is given up
/// (design doc 4.5, condition 2).
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(10);
/// Keyframe requests closer together than this are merged: Connected,
/// MediaReady and a PLI tend to arrive within milliseconds, and each forced
/// IDR is a 100-200 KB burst.
const KEYFRAME_SPACING: Duration = Duration::from_millis(250);

/// What the controller asked for in `SessionAccepted`.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub controller: String,
    pub viewport: Option<Viewport>,
    pub audio: bool,
    pub codec: VideoCodec,
}

/// Host-side choices that do not come from the controller.
#[derive(Debug, Clone)]
pub struct Options {
    /// Bitrate for 1080p60; see [`crate::plan::plan`].
    pub bitrate_budget: u32,
    pub mute_speakers: bool,
}

/// Something the agent has to do on the session's behalf.
#[derive(Debug)]
pub enum Outcome {
    /// Relay to the controller through the server.
    Signal(RtcSignal),
    /// The session cannot continue; tell the server and roll back.
    End(ByeReason),
}

pub struct Session {
    pub id: String,
    controller: String,
    request: Request,
    display: Display,
    plan: StreamPlan,
    relay_ceiling: Option<u32>,
    engine: Engine,
    events: mpsc::UnboundedReceiver<Event>,
    sink: Arc<FrameSink>,
    injector: Injector,
    _mute: Option<SpeakerMute>,
    capture: Option<Capture>,
    starting: Option<JoinHandle<Result<Capture>>>,
    offer_applied: bool,
    early_candidates: Vec<String>,
    relayed: bool,
    down_since: Option<Instant>,
    last_keyframe: Option<Instant>,
    keyframe_owed: bool,
}

impl Session {
    pub async fn start(
        request: Request,
        options: &Options,
        stun: Option<&str>,
        turn: Option<&TurnCredentials>,
    ) -> Result<Session> {
        let display = platform::main_display().context("no display is attached")?;
        let plan = plan(&display, request.viewport.as_ref(), options.bitrate_budget);
        info!(
            "session {} for {}: {}x{}@{} {} at {} kbit/s",
            request.id,
            request.controller,
            plan.width,
            plan.height,
            plan.fps,
            request.codec,
            plan.bitrate / 1000
        );

        let injector = Injector::start(display);
        let input = injector.sender();
        let (events_tx, events) = mpsc::unbounded_channel();
        // Input skips the session loop: it goes straight to the injector
        // thread from the engine's callback.
        let sink = sink(move |event| match event {
            Event::Input { data, .. } => input.submit(data),
            other => {
                let _ = events_tx.send(other);
            }
        });

        let mut config = EngineConfig::new(Role::Host, sink).with_video_codec(request.codec);
        if let Some(stun) = stun.and_then(resolve) {
            config = config.with_stun(stun);
        }
        let relay_ceiling = turn.and_then(|turn| turn.relay_bitrate_ceiling);
        if let Some(turn) = turn {
            if let Some(server) = resolve(&turn.server) {
                config = config.with_turn(TurnConfig {
                    server,
                    username: turn.username.clone(),
                    password: turn.password.clone(),
                    relay_bitrate_ceiling: relay_ceiling.unwrap_or(0),
                });
            }
        }
        let engine = Engine::start(config)
            .await
            .context("starting the transport")?;

        Ok(Session {
            id: request.id.clone(),
            controller: request.controller.clone(),
            request,
            display,
            plan,
            relay_ceiling,
            sink: FrameSink::new(Some(engine.clone())),
            engine,
            events,
            injector,
            _mute: options.mute_speakers.then(SpeakerMute::engage),
            capture: None,
            starting: None,
            offer_applied: false,
            early_candidates: Vec::new(),
            relayed: false,
            down_since: None,
            last_keyframe: None,
            keyframe_owed: false,
        })
    }

    // MARK: inputs from the controller

    pub fn on_signal(&mut self, signal: RtcSignal) {
        match signal {
            RtcSignal::Offer { sdp } => {
                self.engine.accept_offer(sdp);
                self.offer_applied = true;
                // Candidates that arrive before the offer cannot be matched to
                // its credentials, so they waited until now.
                for candidate in self.early_candidates.drain(..) {
                    self.engine.add_remote_candidate(candidate);
                }
                self.start_capture();
            }
            RtcSignal::Ice { candidate, .. } if self.offer_applied => {
                self.engine.add_remote_candidate(candidate)
            }
            RtcSignal::Ice { candidate, .. } => self.early_candidates.push(candidate),
            RtcSignal::Answer { .. } | RtcSignal::IceComplete => {}
        }
    }

    pub fn on_control(&mut self, control: SessionControl) {
        match control {
            SessionControl::RequestKeyframe => self.request_keyframe(),
            SessionControl::SetFrameRate { fps } => {
                let fps = if fps >= 50 { 60 } else { 45 };
                info!("frame rate {fps}");
                self.plan.fps = fps;
                if let Some(capture) = &self.capture {
                    capture.set_frame_rate(fps);
                }
            }
            SessionControl::SetAudioEnabled { enabled } => {
                self.request.audio = enabled;
                if let Some(capture) = &self.capture {
                    capture.set_audio(enabled);
                }
            }
            other => debug!("unsupported session control {other:?}"),
        }
    }

    // MARK: the session's own progress

    /// Waits for the next thing the agent must act on. Cancel safe.
    pub async fn next(&mut self) -> Outcome {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                event = self.events.recv() => match event {
                    Some(event) => if let Some(outcome) = self.on_event(event) {
                        return outcome;
                    },
                    None => return Outcome::End(ByeReason::HostError),
                },
                started = async { self.starting.as_mut().unwrap().await }, if self.starting.is_some() => {
                    self.starting = None;
                    match started {
                        Ok(Ok(capture)) => {
                            info!("capturing");
                            capture.set_bitrate(self.bitrate());
                            capture.set_audio(self.request.audio);
                            self.capture = Some(capture);
                            self.request_keyframe();
                        }
                        Ok(Err(error)) => {
                            warn!("capture failed: {error:#}");
                            return Outcome::End(ByeReason::HostError);
                        }
                        Err(error) => {
                            warn!("capture task failed: {error}");
                            return Outcome::End(ByeReason::HostError);
                        }
                    }
                }
                _ = tick.tick() => {
                    if let Some(since) = self.down_since {
                        if since.elapsed() > RECONNECT_WINDOW {
                            warn!("media path down for {:?}, giving up", since.elapsed());
                            return Outcome::End(ByeReason::NetworkTimeout);
                        }
                    }
                    if self.keyframe_owed {
                        self.request_keyframe();
                    }
                }
            }
        }
    }

    fn on_event(&mut self, event: Event) -> Option<Outcome> {
        match event {
            Event::LocalAnswer(sdp) => return Some(Outcome::Signal(RtcSignal::Answer { sdp })),
            Event::LocalCandidate(candidate) => {
                return Some(Outcome::Signal(RtcSignal::Ice {
                    candidate,
                    sdp_mid: None,
                    sdp_mline_index: None,
                }))
            }
            Event::Connected => {
                if self.down_since.take().is_some() {
                    info!("media path recovered");
                } else {
                    info!("connected to {}", self.controller);
                }
                self.request_keyframe();
            }
            Event::Disconnected => {
                info!("media path lost, waiting up to {RECONNECT_WINDOW:?}");
                self.down_since.get_or_insert_with(Instant::now);
            }
            // The first keyframe is often written before the path is ready,
            // and a static desktop will not produce another on its own.
            Event::MediaReady { .. } | Event::FrameDropped | Event::KeyframeRequest => {
                self.request_keyframe()
            }
            Event::PathUpdate { relayed, .. } => {
                if relayed != self.relayed {
                    self.relayed = relayed;
                    info!("path is {}", if relayed { "relayed" } else { "direct" });
                    if let Some(capture) = &self.capture {
                        capture.set_bitrate(self.bitrate());
                    }
                }
            }
            Event::Closed(reason) => {
                warn!("transport closed: {reason}");
                return Some(Outcome::End(ByeReason::HostError));
            }
            other => debug!("engine: {other:?}"),
        }
        None
    }

    // MARK: teardown

    /// Rolls the machine back to how it was before the session.
    pub fn finish(self, lock: bool) {
        let (video, audio) = self.sink.counts();
        info!(
            "session {} over: {video} video frames, {audio} audio packets sent",
            self.id
        );
        let Session {
            injector,
            capture,
            starting,
            engine,
            _mute,
            ..
        } = self;
        drop(injector);
        if let Some(starting) = starting {
            starting.abort();
        }
        drop(capture);
        engine.shutdown();
        drop(_mute);
        if lock {
            platform::lock_screen();
        }
    }

    // MARK: helpers

    fn start_capture(&mut self) {
        if self.capture.is_some() || self.starting.is_some() {
            return;
        }
        // Started on the offer rather than on SessionAccepted: capture takes
        // about a second to come up and frames before the media sections
        // exist are dropped anyway.
        let (display, plan, codec, audio) = (
            self.display,
            self.plan,
            self.request.codec,
            self.request.audio,
        );
        let sink = self.sink.clone();
        self.starting = Some(tokio::spawn(async move {
            Capture::start(&display, &plan, codec, audio, sink).await
        }));
    }

    fn request_keyframe(&mut self) {
        let Some(capture) = &self.capture else {
            // The first frame of a new capture is a keyframe anyway.
            return;
        };
        if self
            .last_keyframe
            .is_some_and(|at| at.elapsed() < KEYFRAME_SPACING)
        {
            // Deferred, not dropped: the tick fires it once the spacing passed.
            self.keyframe_owed = true;
            return;
        }
        self.keyframe_owed = false;
        self.last_keyframe = Some(Instant::now());
        capture.request_keyframe();
    }

    fn bitrate(&self) -> u32 {
        match (self.relayed, self.relay_ceiling) {
            (true, Some(ceiling)) if ceiling > 0 => self.plan.bitrate.min(ceiling),
            _ => self.plan.bitrate,
        }
    }
}

fn resolve(authority: &str) -> Option<SocketAddr> {
    authority
        .to_socket_addrs()
        .ok()?
        .find(SocketAddr::is_ipv4)
        .or_else(|| authority.to_socket_addrs().ok()?.next())
}
