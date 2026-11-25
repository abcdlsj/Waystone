//! One stream to one controller, from `SessionAccepted` to rollback.
//!
//! A [`Session`] owns everything a stream touches: the WebRTC engine, the
//! capture session, the input injector and the speaker mute. Ending it is
//! [`Session::finish`], which undoes all of it in a fixed order (design doc
//! 4.5): release held input, stop capture, close the transport, restore the
//! speakers, then lock the screen if asked to.
//!
//! On Windows, [`SessionMode::ConsoleWorker`] delegates capture, audio, input,
//! speaker mute and screen locking to a privileged console-session worker
//! spawned from the Session 0 service, while keeping the WebRTC engine and
//! deterministic rollback inside Session 0.

use std::net::{SocketAddr, ToSocketAddrs};
#[cfg(windows)]
use std::path::PathBuf;
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

/// How a session accesses the desktop, audio and input subsystems.
#[derive(Debug, Clone, Default)]
pub enum SessionMode {
    /// Capture, inject, mute and lock in-process (macOS and Windows foreground mode).
    #[default]
    Direct,
    /// Delegate console-session media, input, mute and lock to a privileged
    /// child process spawned in the active physical console session (Windows Session 0).
    #[cfg(windows)]
    ConsoleWorker { state_dir: PathBuf },
}

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

enum SessionBackend {
    Direct {
        injector: Injector,
        mute: Option<SpeakerMute>,
        capture: Option<Capture>,
        starting: Option<JoinHandle<Result<Capture>>>,
    },
    #[cfg(windows)]
    Worker {
        worker: Arc<platform::worker::WorkerSession>,
        capture_active: bool,
        starting: Option<JoinHandle<Result<()>>>,
        dead_rx: mpsc::UnboundedReceiver<String>,
    },
}

pub struct Session {
    pub id: String,
    controller: String,
    request: Request,
    display: Display,
    plan: StreamPlan,
    bitrate_budget: u32,
    relay_ceiling: Option<u32>,
    engine: Engine,
    events: mpsc::UnboundedReceiver<Event>,
    sink: Arc<FrameSink>,
    backend: SessionBackend,
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
        mode: &SessionMode,
        stun: Option<&str>,
        turn: Option<&TurnCredentials>,
    ) -> Result<Session> {
        let (events_tx, events) = mpsc::unbounded_channel();

        let (display, backend, engine_sink) = match mode {
            SessionMode::Direct => {
                let display = platform::main_display().context("no display is attached")?;
                let injector = Injector::start(display);
                let input = injector.sender();
                let mute = options.mute_speakers.then(SpeakerMute::engage);
                // Input skips the session loop: it goes straight to the injector
                // thread from the engine's callback.
                let engine_sink = sink(move |event| match event {
                    Event::Input { data, .. } => input.submit(data),
                    other => {
                        let _ = events_tx.send(other);
                    }
                });
                let backend = SessionBackend::Direct {
                    injector,
                    mute,
                    capture: None,
                    starting: None,
                };
                (display, backend, engine_sink)
            }
            #[cfg(windows)]
            SessionMode::ConsoleWorker { state_dir } => {
                let (worker, dead_rx) = platform::worker::WorkerSession::start(
                    state_dir.clone(),
                    options.mute_speakers,
                )
                .await
                .context("starting privileged console worker")?;
                let display = worker.display();
                let input = worker.input_sender();
                let engine_sink = sink(move |event| match event {
                    Event::Input { data, .. } => input.submit(data),
                    other => {
                        let _ = events_tx.send(other);
                    }
                });
                let backend = SessionBackend::Worker {
                    worker: Arc::new(worker),
                    capture_active: false,
                    starting: None,
                    dead_rx,
                };
                (display, backend, engine_sink)
            }
        };

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

        let mut config = EngineConfig::new(Role::Host, engine_sink)
            .with_video_codec(request.codec)
            .with_port_mapping(true);
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
            bitrate_budget: options.bitrate_budget,
            relay_ceiling,
            sink: FrameSink::new(Some(engine.clone())),
            engine,
            events,
            backend,
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
                let fps = Viewport {
                    width: self.plan.width,
                    height: self.plan.height,
                    dpi: 0,
                    fps,
                }
                .frame_rate_tier();
                let updated = plan(
                    &self.display,
                    Some(&Viewport {
                        width: self.plan.width,
                        height: self.plan.height,
                        dpi: 0,
                        fps,
                    }),
                    self.bitrate_budget,
                );
                info!("frame rate {fps} ({} kbit/s)", updated.bitrate / 1000);
                self.plan.fps = fps;
                self.plan.bitrate = updated.bitrate;
                self.apply_frame_rate(fps);
                self.apply_bitrate(self.bitrate());
            }
            SessionControl::SetResolution { width, height } => {
                let updated = plan(
                    &self.display,
                    Some(&Viewport {
                        width,
                        height,
                        dpi: 0,
                        fps: self.plan.fps,
                    }),
                    self.bitrate_budget,
                );
                if (updated.width, updated.height) == (self.plan.width, self.plan.height) {
                    return;
                }
                info!(
                    "resolution {}x{}@{} ({} kbit/s)",
                    updated.width,
                    updated.height,
                    updated.fps,
                    updated.bitrate / 1000
                );
                self.plan = updated;
                self.apply_resolution(updated.width, updated.height, self.bitrate());
            }
            SessionControl::SetAudioEnabled { enabled } => {
                self.request.audio = enabled;
                self.apply_audio(enabled);
            }
            other => debug!("unsupported session control {other:?}"),
        }
    }

    // MARK: the session's own progress

    /// Waits for the next thing the agent must act on. Cancel safe.
    pub async fn next(&mut self) -> Outcome {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            match &mut self.backend {
                SessionBackend::Direct {
                    capture, starting, ..
                } => {
                    tokio::select! {
                        event = self.events.recv() => match event {
                            Some(event) => if let Some(outcome) = self.on_event(event) {
                                return outcome;
                            },
                            None => return Outcome::End(ByeReason::HostError),
                        },
                        started = async { starting.as_mut().unwrap().await }, if starting.is_some() => {
                            *starting = None;
                            match started {
                                Ok(Ok(started_capture)) => {
                                    info!("capturing");
                                    let bitrate = match (self.relayed, self.relay_ceiling) {
                                        (true, Some(ceiling)) if ceiling > 0 => self.plan.bitrate.min(ceiling),
                                        _ => self.plan.bitrate,
                                    };
                                    started_capture.set_bitrate(bitrate);
                                    started_capture.set_audio(self.request.audio);
                                    *capture = Some(started_capture);
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
                #[cfg(windows)]
                SessionBackend::Worker {
                    worker,
                    capture_active,
                    starting,
                    dead_rx,
                } => {
                    tokio::select! {
                        event = self.events.recv() => match event {
                            Some(event) => if let Some(outcome) = self.on_event(event) {
                                return outcome;
                            },
                            None => return Outcome::End(ByeReason::HostError),
                        },
                        started = async { starting.as_mut().unwrap().await }, if starting.is_some() => {
                            *starting = None;
                            match started {
                                Ok(Ok(())) => {
                                    info!("capturing (privileged console worker)");
                                    let bitrate = match (self.relayed, self.relay_ceiling) {
                                        (true, Some(ceiling)) if ceiling > 0 => self.plan.bitrate.min(ceiling),
                                        _ => self.plan.bitrate,
                                    };
                                    worker.set_bitrate(bitrate);
                                    worker.set_audio(self.request.audio);
                                    *capture_active = true;
                                    self.request_keyframe();
                                }
                                Ok(Err(error)) => {
                                    warn!("worker capture failed: {error:#}");
                                    return Outcome::End(ByeReason::HostError);
                                }
                                Err(error) => {
                                    warn!("worker capture task failed: {error}");
                                    return Outcome::End(ByeReason::HostError);
                                }
                            }
                        }
                        dead = dead_rx.recv() => {
                            let reason = dead.unwrap_or_else(|| "console worker exited".into());
                            warn!("console worker failed mid-session: {reason}");
                            return Outcome::End(ByeReason::HostError);
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
                    let bitrate = self.bitrate();
                    self.apply_bitrate(bitrate);
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
    ///
    /// Order is fixed (design doc 4.5): release held input -> stop capture ->
    /// close transport -> restore speakers -> lock screen (if requested).
    pub fn finish(self, lock: bool) {
        let (video, audio) = self.sink.counts();
        info!(
            "session {} over: {video} video frames, {audio} audio packets sent",
            self.id
        );
        let Session {
            engine, backend, ..
        } = self;
        match backend {
            SessionBackend::Direct {
                injector,
                mute,
                capture,
                starting,
            } => {
                drop(injector);
                if let Some(starting) = starting {
                    starting.abort();
                }
                drop(capture);
                engine.shutdown();
                drop(mute);
                if lock {
                    platform::lock_screen();
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker, starting, ..
            } => {
                worker.release_input_blocking();
                if let Some(starting) = starting {
                    starting.abort();
                }
                worker.stop_capture_blocking();
                engine.shutdown();
                worker.restore_speakers_blocking();
                if lock {
                    worker.lock_screen_blocking();
                }
                if let Ok(owned) = Arc::try_unwrap(worker) {
                    owned.shutdown_blocking();
                }
            }
        }
    }

    // MARK: helpers

    fn start_capture(&mut self) {
        let (display, plan, codec, audio) = (
            self.display,
            self.plan,
            self.request.codec,
            self.request.audio,
        );
        let sink = self.sink.clone();
        match &mut self.backend {
            SessionBackend::Direct {
                capture, starting, ..
            } => {
                if capture.is_some() || starting.is_some() {
                    return;
                }
                // Started on the offer rather than on SessionAccepted: capture takes
                // about a second to come up and frames before the media sections
                // exist are dropped anyway.
                *starting = Some(tokio::spawn(async move {
                    Capture::start(&display, &plan, codec, audio, sink).await
                }));
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker,
                capture_active,
                starting,
                ..
            } => {
                if *capture_active || starting.is_some() {
                    return;
                }
                let worker = worker.clone();
                *starting = Some(tokio::spawn(async move {
                    worker
                        .start_capture(display.id, plan, codec, audio, sink)
                        .await
                }));
            }
        }
    }

    fn capture_is_running(&self) -> bool {
        match &self.backend {
            SessionBackend::Direct { capture, .. } => capture.is_some(),
            #[cfg(windows)]
            SessionBackend::Worker { capture_active, .. } => *capture_active,
        }
    }

    fn request_keyframe(&mut self) {
        if !self.capture_is_running() {
            // The first frame of a new capture is a keyframe anyway.
            return;
        }
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
        match &self.backend {
            SessionBackend::Direct { capture, .. } => {
                if let Some(capture) = capture {
                    capture.request_keyframe();
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker { worker, .. } => worker.request_keyframe(),
        }
    }

    fn apply_bitrate(&self, bitrate: u32) {
        match &self.backend {
            SessionBackend::Direct { capture, .. } => {
                if let Some(capture) = capture {
                    capture.set_bitrate(bitrate);
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker,
                capture_active,
                ..
            } => {
                if *capture_active {
                    worker.set_bitrate(bitrate);
                }
            }
        }
    }

    fn apply_frame_rate(&self, fps: u32) {
        match &self.backend {
            SessionBackend::Direct { capture, .. } => {
                if let Some(capture) = capture {
                    capture.set_frame_rate(fps);
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker,
                capture_active,
                ..
            } => {
                if *capture_active {
                    worker.set_frame_rate(fps);
                }
            }
        }
    }

    fn apply_resolution(&mut self, width: u32, height: u32, bitrate: u32) {
        let mut restart = false;
        match &mut self.backend {
            SessionBackend::Direct { capture, .. } => {
                if let Some(active) = capture {
                    if !active.set_resolution(width, height, bitrate) {
                        *capture = None;
                        restart = true;
                    }
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker,
                capture_active,
                ..
            } => {
                if *capture_active {
                    worker.set_resolution(width, height, bitrate);
                }
            }
        }
        if restart {
            self.start_capture();
        }
    }

    fn apply_audio(&self, enabled: bool) {
        match &self.backend {
            SessionBackend::Direct { capture, .. } => {
                if let Some(capture) = capture {
                    capture.set_audio(enabled);
                }
            }
            #[cfg(windows)]
            SessionBackend::Worker {
                worker,
                capture_active,
                ..
            } => {
                if *capture_active {
                    worker.set_audio(enabled);
                }
            }
        }
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
