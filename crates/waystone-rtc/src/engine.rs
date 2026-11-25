//! The single-owner driver around a `str0m::Rtc` session.
//!
//! `str0m` is sans-IO and has one hard rule: every mutation must be followed by
//! draining `poll_output` until `Output::Timeout`. To make that rule impossible to
//! break from the outside, exactly one task owns the [`Rtc`]. Other threads post
//! commands into a channel and receive events through a callback.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, Input, Output, Rtc, RtcConfig,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{watch, Notify};
use tracing::{debug, info, warn};

use waystone_proto::Viewport;

use crate::pacer::{self, MediaPacer};
use crate::punch::{self, HostSpray, PunchPacket, PunchedSocket};
use crate::stun::{class, method, Builder, Message};
use crate::turn::{Event as TurnEvent, Received as TurnReceived, TurnClient};

pub use waystone_proto::VideoCodec;

/// The str0m codec a [`VideoCodec`] maps to.
fn str0m_codec(codec: VideoCodec) -> Codec {
    match codec {
        VideoCodec::H265 => Codec::H265,
        VideoCodec::H264 => Codec::H264,
    }
}

/// Which end of a session this engine drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Sends video and audio, receives input frames.
    Host,
    /// Receives video and audio, sends input frames.
    Controller,
}

/// Options chosen by the controller when it starts a session.
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    pub viewport: Viewport,
    pub audio: bool,
    /// Late bound bitrate ceiling in bits per second.
    pub max_video_bitrate: u32,
}

impl SessionOptions {
    /// Builds options for a controller viewport with Waystone's default ceiling.
    pub fn new(viewport: Viewport, audio: bool) -> Self {
        SessionOptions {
            viewport,
            audio,
            max_video_bitrate: 8_000_000,
        }
    }
}

/// Everything the owner of an engine needs to react to.
#[derive(Debug, Clone)]
pub enum Event {
    /// SDP offer produced by the controller, to be relayed by the server.
    LocalOffer(String),
    /// SDP answer produced by the host, to be relayed by the server.
    LocalAnswer(String),
    /// A local ICE candidate in SDP form, to be relayed by the server.
    LocalCandidate(String),
    /// ICE and DTLS are up.
    Connected,
    /// The transport went away; the session is over.
    Disconnected,
    /// Negotiated media ids, emitted once the session is usable.
    MediaReady {
        video: Option<Mid>,
        audio: Option<Mid>,
    },
    /// One encoded video frame from the remote peer, in Annex-B form.
    VideoFrame {
        data: Vec<u8>,
        keyframe: bool,
        rtp_time: u64,
    },
    /// One Opus packet from the remote peer, 48 kHz timestamps.
    AudioFrame { data: Vec<u8>, rtp_time: u64 },
    /// Input frame from the remote controller (host side only).
    Input { reliable: bool, data: Vec<u8> },
    /// A data channel opened.
    ChannelOpen(String),
    /// Current path and health of the session.
    ///
    /// Emitted as soon as media starts flowing on a new candidate pair and
    /// once per second with fresh statistics. `relayed` is true when the
    /// selected pair uses a relay candidate on either side; the host clamps its
    /// encoder to [`TurnConfig::relay_bitrate_ceiling`] while it is set.
    PathUpdate {
        rtt_ms: Option<u32>,
        relayed: bool,
        estimated_bitrate: Option<u32>,
    },
    /// The remote peer asked for a fresh keyframe.
    KeyframeRequest,
    /// A video frame was dropped, because the session is not ready yet or the
    /// engine fell behind and discarded the oldest queued frame.
    ///
    /// The host uses this to retry the keyframe request: the encoder produces a
    /// static frame only once, so dropping it silently would leave the viewer
    /// with a black screen forever.
    FrameDropped,
    /// The session is closed. Fatal errors are reported here too.
    Closed(String),
}

/// Callback invoked for every [`Event`], from the engine task.
pub type EventSink = std::sync::Arc<dyn Fn(Event) + Send + Sync>;

/// Wiring for the built-in relay.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub server: SocketAddr,
    pub username: String,
    pub password: String,
    /// Video bitrate ceiling for relayed sessions.
    ///
    /// The engine does not encode video, so it cannot enforce this itself: it
    /// reports the path through [`Event::PathUpdate`] (`relayed` is true when the
    /// selected candidate pair uses a relay candidate on either side) and the
    /// application clamps its encoder to this value while relayed. When the
    /// `WAYSTONE_BWE` estimator is enabled the engine also caps its target.
    pub relay_bitrate_ceiling: u32,
}

/// Engine configuration.
#[derive(Clone)]
pub struct EngineConfig {
    pub role: Role,
    /// Controller options; the host ignores this.
    pub session: Option<SessionOptions>,
    /// STUN server used to discover a server reflexive candidate.
    pub stun: Option<SocketAddr>,
    pub turn: Option<TurnConfig>,
    /// Local address to bind, `0.0.0.0:0` by default.
    ///
    /// Without one the engine also opens an IPv6 socket, so peers that both
    /// have a global IPv6 address can connect without NAT traversal.
    pub bind: Option<SocketAddr>,
    /// Codec the host encodes and the controller expects.
    pub video_codec: VideoCodec,
    /// Gather and advertise only the relay candidate.
    ///
    /// Useful behind NATs that break hole punching (and for tests); requires
    /// [`EngineConfig::turn`].
    pub relay_only: bool,
    /// Ask the router to forward a public port to the engine (UPnP IGD, PCP
    /// or NAT-PMP) and advertise it as a server reflexive candidate.
    ///
    /// Meant for the host: it makes the host reachable from behind a NAT that
    /// cannot be hole punched (a phone on cellular, typically). Needs the
    /// `port-mapping` feature; ignored without it.
    pub port_mapping: bool,
    pub sink: EventSink,
}

impl EngineConfig {
    pub fn new(role: Role, sink: EventSink) -> Self {
        EngineConfig {
            role,
            session: None,
            stun: None,
            turn: None,
            bind: None,
            video_codec: VideoCodec::H265,
            relay_only: false,
            port_mapping: false,
            sink,
        }
    }

    pub fn with_video_codec(mut self, codec: VideoCodec) -> Self {
        self.video_codec = codec;
        self
    }

    pub fn with_session(mut self, options: SessionOptions) -> Self {
        self.session = Some(options);
        self
    }

    pub fn with_stun(mut self, server: SocketAddr) -> Self {
        self.stun = Some(server);
        self
    }

    pub fn with_turn(mut self, turn: TurnConfig) -> Self {
        self.turn = Some(turn);
        self
    }

    pub fn with_bind(mut self, bind: SocketAddr) -> Self {
        self.bind = Some(bind);
        self
    }

    /// Only gathers and advertises the TURN relay candidate.
    pub fn with_relay_only(mut self, relay_only: bool) -> Self {
        self.relay_only = relay_only;
        self
    }

    /// Requests a router port mapping, see [`EngineConfig::port_mapping`].
    pub fn with_port_mapping(mut self, port_mapping: bool) -> Self {
        self.port_mapping = port_mapping;
        self
    }
}

/// Data channel label for high frequency pointer samples.
pub const CHANNEL_INPUT_FAST: &str = "input-fast";
/// Data channel label for reliable state edges.
pub const CHANNEL_INPUT_RELIABLE: &str = "input-reliable";

/// Video frames waiting for the engine task. Older frames are dropped first:
/// a stale frame is worth less than the newest one, and the drop is reported
/// as [`Event::FrameDropped`] so the host can ask for a keyframe.
const VIDEO_QUEUE: usize = 8;
/// Audio packets and input frames waiting for the engine task.
const MEDIA_QUEUE: usize = 512;

/// Control commands. These are rare and must never be lost, so their channel
/// is unbounded; media has bounded queues of its own.
enum Command {
    Offer,
    AcceptOffer(String),
    AcceptAnswer(String),
    RemoteCandidate(String),
    SetVideoBitrate(u32),
    Shutdown,
}

enum Media {
    Audio { data: Vec<u8>, rtp_time: u64 },
    Input { reliable: bool, data: Vec<u8> },
}

#[derive(Default)]
struct VideoQueue {
    frames: Mutex<VecDeque<(Vec<u8>, u64)>>,
    dropped: AtomicBool,
    notify: Notify,
}

impl VideoQueue {
    fn push(&self, data: Vec<u8>, rtp_time: u64) {
        {
            let mut frames = self.frames.lock().unwrap_or_else(|e| e.into_inner());
            if frames.len() >= VIDEO_QUEUE {
                frames.pop_front();
                self.dropped.store(true, Ordering::Relaxed);
            }
            frames.push_back((data, rtp_time));
        }
        self.notify.notify_one();
    }

    fn take(&self) -> VecDeque<(Vec<u8>, u64)> {
        std::mem::take(&mut *self.frames.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Handle used by the rest of the application.
#[derive(Clone)]
pub struct Engine {
    tx: UnboundedSender<Command>,
    media: mpsc::Sender<Media>,
    video: Arc<VideoQueue>,
    local_addr: SocketAddr,
    stopped: watch::Receiver<bool>,
}

impl Engine {
    /// Binds the transport socket and spawns the driver task.
    pub async fn start(config: EngineConfig) -> std::io::Result<Engine> {
        let bind = config.bind.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
        // A single socket serves every local interface, so host candidates only
        // differ in their IP address; the destination address of each datagram is
        // read from the ancillary data instead.
        let socket = UdpSocket::bind(bind).await?;
        crate::net::configure_udp_buffers(&socket);
        if let Err(error) = crate::net::enable_destination_info(&socket) {
            warn!("cannot report datagram destinations: {error}");
        }
        let local_addr = socket.local_addr()?;
        let socket6 = if config.bind.is_none() && !config.relay_only {
            match bind_ipv6() {
                Ok(socket) => Some(socket),
                Err(error) => {
                    debug!("no IPv6 socket: {error}");
                    None
                }
            }
        } else {
            None
        };
        let (tx, rx) = unbounded_channel();
        let (media, media_rx) = mpsc::channel(MEDIA_QUEUE);
        let (stopped_tx, stopped) = watch::channel(false);
        let video = Arc::new(VideoQueue::default());
        let queue = video.clone();
        tokio::spawn(async move {
            let mut driver = Driver::new(config, socket, socket6, rx, media_rx, queue, local_addr);
            driver.run().await;
            let _ = stopped_tx.send(true);
        });
        Ok(Engine {
            tx,
            media,
            video,
            local_addr,
            stopped,
        })
    }

    /// The UDP address the engine is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Creates the SDP offer (controller side).
    pub fn offer(&self) {
        let _ = self.tx.send(Command::Offer);
    }

    /// Accepts the controller offer (host side).
    pub fn accept_offer(&self, sdp: String) {
        let _ = self.tx.send(Command::AcceptOffer(sdp));
    }

    /// Applies the host answer (controller side).
    pub fn accept_answer(&self, sdp: String) {
        let _ = self.tx.send(Command::AcceptAnswer(sdp));
    }

    /// Adds a trickled remote candidate received from the signaling channel.
    pub fn add_remote_candidate(&self, candidate: String) {
        let _ = self.tx.send(Command::RemoteCandidate(candidate));
    }

    /// Writes one encoded video frame.
    ///
    /// `data` is Annex-B and must carry the parameter sets when `keyframe` is set.
    /// When the engine falls behind, the oldest queued frame is dropped and
    /// [`Event::FrameDropped`] is reported.
    pub fn send_video(&self, data: Vec<u8>, rtp_time: u64) {
        self.video.push(data, rtp_time);
    }

    /// Writes one Opus packet with a 48 kHz timestamp. Dropped when the queue
    /// is full.
    pub fn send_audio(&self, data: Vec<u8>, rtp_time: u64) {
        if self
            .media
            .try_send(Media::Audio { data, rtp_time })
            .is_err()
        {
            debug!("audio queue full, dropping a packet");
        }
    }

    /// Sends an encoded input frame on the matching data channel. Dropped when
    /// the queue is full.
    pub fn send_input(&self, reliable: bool, data: Vec<u8>) {
        if self
            .media
            .try_send(Media::Input { reliable, data })
            .is_err()
        {
            debug!("input queue full, dropping a frame");
        }
    }

    /// Caps the outgoing video bitrate; the encoder follows this value.
    pub fn set_video_bitrate(&self, bits_per_second: u32) {
        let _ = self.tx.send(Command::SetVideoBitrate(bits_per_second));
    }

    /// Tears the session down.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }

    /// Resolves once the engine task has finished (after [`Event::Closed`]).
    pub async fn stopped(&self) {
        let mut stopped = self.stopped.clone();
        // An error means the task is gone, which is just as final.
        let _ = stopped.wait_for(|done| *done).await;
    }
}

/// Server reflexive discovery in flight. It runs alongside the session instead
/// of before it, so the offer is never delayed; the candidate is trickled when
/// the answer arrives.
struct ReflexiveProbe {
    server: SocketAddr,
    transaction_id: [u8; 12],
    request: Vec<u8>,
    next_send: Instant,
    rto: Duration,
    transmissions: u32,
}

const REFLEXIVE_TRANSMISSIONS: u32 = 4;
const REFLEXIVE_RTO: Duration = Duration::from_millis(250);
/// Tighter receive reordering windows than str0m's 30-frame default (~500 ms)
/// so unrecoverable loss triggers PLI recovery quickly instead of stalling.
const REORDERING_SIZE_VIDEO: usize = 8;
const REORDERING_SIZE_AUDIO: usize = 6;
/// Minimum spacing between receiver-initiated RTCP PLI requests.
const PLI_MIN_INTERVAL: Duration = Duration::from_millis(300);

struct Driver {
    config: EngineConfig,
    socket: UdpSocket,
    /// The IPv6 socket next to the default `0.0.0.0` one, with its address.
    socket6: Option<(UdpSocket, SocketAddr)>,
    /// Winning ephemeral IPv4 socket from NAT3<->NAT4 birthday hole punching.
    punch_socket: Option<(UdpSocket, SocketAddr)>,
    rx: UnboundedReceiver<Command>,
    media_rx: mpsc::Receiver<Media>,
    video: Arc<VideoQueue>,
    rtc: Rtc,
    local_addr: SocketAddr,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    fast_channel: Option<ChannelId>,
    reliable_channel: Option<ChannelId>,
    /// Pending offer handle, kept between `apply()` and `accept_answer()`.
    pending_offer: Option<str0m::change::SdpPendingOffer>,
    turn: Option<TurnClient>,
    reflexive: Option<ReflexiveProbe>,
    /// Remote relay candidates, used to tell whether the selected pair is
    /// relayed on the remote side.
    remote_relays: HashSet<SocketAddr>,
    /// Peer's public IPv4 server-reflexive address, used if the session falls
    /// back to a TURN relay and attempts NAT3<->NAT4 birthday punching.
    remote_srflx: Option<SocketAddr>,
    /// Number of birthday-punch rounds started in this session.
    punch_attempts: u8,
    /// Scheduled time for the next birthday-punch retry while relayed.
    punch_next_retry: Option<Instant>,
    /// Session-derived token shared via the SDP offer's ICE credentials.
    punch_token: Option<[u8; 8]>,
    /// Host-side paced NAT3 pinhole spray.
    host_spray: Option<HostSpray>,
    /// Controller-side pending birthday punch result.
    punch_rx: Option<tokio::sync::oneshot::Receiver<Option<PunchedSocket>>>,
    /// Cancels an in-flight controller burst when direct path wins or on stop.
    punch_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Smooths outgoing large video RTP bursts on the host side.
    pacer: MediaPacer,
    /// True after str0m reports a non-contiguous video frame; drops P-frames
    /// until a fresh keyframe lands so the decoder does not stall on corrupt refs.
    video_awaiting_keyframe: bool,
    last_pli: Option<Instant>,
    /// Local and remote address of the pair media currently flows on.
    path: Option<(SocketAddr, SocketAddr)>,
    relayed: bool,
    rtt_ms: Option<u32>,
    estimated_bitrate: Option<u32>,
    /// Local address each wildcard-socket peer is reached through, for
    /// platforms that do not report datagram destinations.
    routes: HashMap<SocketAddr, IpAddr>,
    bwe_enabled: bool,
    video_bitrate: u32,
    /// Dropped frames, so the warning is logged a bounded number of times.
    video_drop_logged: u64,
    /// Result of the router port mapping, while it is being requested.
    mapping: Option<tokio::sync::oneshot::Receiver<Option<(SocketAddr, SocketAddr)>>>,
    /// Dropping this releases the router port mapping.
    _mapping_lease: Option<tokio::sync::oneshot::Sender<()>>,
    connected: bool,
    closed: bool,
}

impl Driver {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: EngineConfig,
        socket: UdpSocket,
        socket6: Option<UdpSocket>,
        rx: UnboundedReceiver<Command>,
        media_rx: mpsc::Receiver<Media>,
        video: Arc<VideoQueue>,
        local_addr: SocketAddr,
    ) -> Self {
        // Both HEVC and H.264 are offered: HEVC is what every supported iPhone,
        // iPad and Mac decodes in hardware and is the preferred codec, while
        // H.264 is the universal fallback (the iOS simulator, for instance, has
        // no HEVC decoder at all). Opus is the only audio codec.
        let mut rtc_config = RtcConfig::new().clear_codecs();
        match config.video_codec {
            VideoCodec::H265 => rtc_config.codec_config().enable_h265(true),
            VideoCodec::H264 => rtc_config.codec_config().enable_h264(true),
        }
        rtc_config.codec_config().enable_opus(true, false);
        // BWE is opt-in: without feedback the pacer would throttle the video to
        // the initial estimate, so the default is a fixed high ceiling with the
        // encoder doing the rate control and our own MediaPacer smoothing bursts.
        let rtc_config = rtc_config
            .set_stats_interval(Some(Duration::from_secs(1)))
            .set_reordering_size_video(REORDERING_SIZE_VIDEO)
            .set_reordering_size_audio(REORDERING_SIZE_AUDIO);
        let bwe_enabled = std::env::var_os("WAYSTONE_BWE").is_some();
        let rtc_config = if bwe_enabled {
            rtc_config.enable_bwe(Some(str0m::bwe::Bitrate::mbps(2)))
        } else {
            rtc_config
        };

        let video_bitrate = config
            .session
            .map(|session| session.max_video_bitrate)
            .unwrap_or(8_000_000);
        let now = Instant::now();

        Driver {
            rtc: rtc_config.build(now),
            config,
            socket,
            socket6: socket6.and_then(|socket| {
                let address = socket.local_addr().ok()?;
                Some((socket, address))
            }),
            punch_socket: None,
            rx,
            media_rx,
            video,
            local_addr,
            video_mid: None,
            audio_mid: None,
            fast_channel: None,
            reliable_channel: None,
            pending_offer: None,
            turn: None,
            reflexive: None,
            remote_relays: HashSet::new(),
            remote_srflx: None,
            punch_attempts: 0,
            punch_next_retry: None,
            punch_token: None,
            host_spray: None,
            punch_rx: None,
            punch_cancel: None,
            pacer: MediaPacer::new(now, video_bitrate),
            video_awaiting_keyframe: false,
            last_pli: None,
            path: None,
            relayed: false,
            rtt_ms: None,
            estimated_bitrate: None,
            routes: HashMap::new(),
            bwe_enabled,
            video_bitrate,
            video_drop_logged: 0,
            mapping: None,
            _mapping_lease: None,
            connected: false,
            closed: false,
        }
    }

    async fn run(&mut self) {
        self.gather_candidates().await;
        self.declare_local_media();

        let mut buf = vec![0u8; 2048];
        let mut buf6 = vec![0u8; 2048];
        let mut buf_punch = vec![0u8; 2048];
        loop {
            if self.closed {
                break;
            }

            let deadline = self.drain().await;
            let sleep = tokio::time::sleep_until(deadline.into());
            tokio::pin!(sleep);

            // Deliberately not `biased`: with a video frame arriving every 16 ms
            // a biased select would drain commands first and starve the socket,
            // so TWCC feedback and NACKs would never be processed and the
            // receiver would keep waiting for a keyframe that already went out.
            tokio::select! {
                command = self.rx.recv() => match command {
                    Some(command) => self.handle_command(command),
                    None => break,
                },
                Some(media) = self.media_rx.recv() => self.handle_media(media),
                _ = self.video.notify.notified() => self.write_queued_video(),
                received = crate::net::recv_with_destination(&self.socket, &mut buf) => match received {
                    Ok(datagram) => {
                        let len = datagram.len;
                        let local = self.local_addr;
                        self.handle_datagram(&buf[..len], datagram.source, datagram.destination, local);
                    }
                    Err(error) if is_transient(&error) => {
                        // An ICMP error from an earlier send (a dead candidate
                        // pair) surfaces here on some platforms.
                        debug!("socket read failed: {error}");
                    }
                    Err(error) => {
                        warn!("socket read failed: {error}");
                        break;
                    }
                },
                received = recv_optional(self.socket6.as_ref(), &mut buf6) => match received {
                    Ok((datagram, local)) => {
                        let len = datagram.len;
                        self.handle_datagram(&buf6[..len], datagram.source, datagram.destination, local);
                    }
                    Err(error) if is_transient(&error) => debug!("ipv6 socket read failed: {error}"),
                    Err(error) => {
                        // The IPv4 socket still carries the session.
                        warn!("ipv6 socket read failed, closing it: {error}");
                        self.socket6 = None;
                    }
                },
                received = recv_optional(self.punch_socket.as_ref(), &mut buf_punch) => match received {
                    Ok((datagram, local)) => {
                        let len = datagram.len;
                        self.handle_datagram(&buf_punch[..len], datagram.source, datagram.destination, local);
                    }
                    Err(error) if is_transient(&error) => debug!("punched socket read failed: {error}"),
                    Err(error) => {
                        warn!("punched socket read failed, closing it: {error}");
                        self.punch_socket = None;
                    }
                },
                punched = wait_punch(&mut self.punch_rx) => {
                    self.punch_rx = None;
                    self.punch_cancel = None;
                    if let Some(punched) = punched {
                        self.install_punched_socket(punched);
                    } else {
                        self.schedule_punch_retry(Instant::now());
                    }
                }
                mapped = wait_mapping(&mut self.mapping) => {
                    self.mapping = None;
                    if let Some((external, base)) = mapped {
                        match Candidate::server_reflexive(external, base, "udp") {
                            Ok(candidate) => self.add_local_candidate(candidate),
                            Err(error) => warn!("mapped candidate rejected: {error}"),
                        }
                    }
                }
                _ = &mut sleep => self.handle_timeout(Instant::now()),
            }
        }

        // Cancel any in-flight birthday punch before tearing down.
        self.punch_cancel = None;
        self.punch_next_retry = None;
        self.host_spray = None;

        // Release the allocation instead of letting it linger for ten minutes.
        if let Some(turn) = self.turn.as_mut() {
            turn.close(Instant::now());
        }
        self.flush_turn().await;

        (self.config.sink)(Event::Closed("engine stopped".into()));
        info!("engine task finished");
    }

    fn handle_timeout(&mut self, now: Instant) {
        self.rtc.handle_input(Input::Timeout(now)).ok();
        if let Some(turn) = self.turn.as_mut() {
            turn.handle_timeout(now);
        }
        if let Some(spray) = self.host_spray.as_mut() {
            spray.step(&self.socket, now);
            if spray.is_done() {
                self.host_spray = None;
                self.schedule_punch_retry(now);
            }
        }
        if let Some(retry_at) = self.punch_next_retry {
            if now >= retry_at {
                self.punch_next_retry = None;
                self.maybe_start_punch();
            }
        }
        if let Some(probe) = self.reflexive.as_mut() {
            if now >= probe.next_send {
                if probe.transmissions >= REFLEXIVE_TRANSMISSIONS {
                    warn!("stun discovery via {} timed out", probe.server);
                    self.reflexive = None;
                } else {
                    probe.transmissions += 1;
                    probe.rto *= 2;
                    probe.next_send = now + probe.rto;
                    let (request, server) = (probe.request.clone(), probe.server);
                    if let Err(error) = self.socket_for(server).try_send_to(&request, server) {
                        debug!("stun request to {server} failed: {error}");
                    }
                }
            }
        }
    }

    /// Drains every queued output and returns the next wake-up deadline.
    ///
    /// This performs the socket writes directly: the engine task is the only
    /// owner of the `Rtc`, so it may await here without breaking the invariant.
    async fn drain(&mut self) -> Instant {
        loop {
            self.flush_pacer().await;
            // TURN events can add a local candidate, which is an `Rtc` mutation,
            // so they are handled before polling the session again.
            self.flush_turn().await;
            if self.handle_turn_events() {
                continue;
            }
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    self.flush_pacer().await;
                    let turn = self.turn.as_ref().and_then(|turn| turn.poll_timeout());
                    let probe = self.reflexive.as_ref().map(|probe| probe.next_send);
                    let spray = self.host_spray.as_ref().and_then(HostSpray::next_deadline);
                    let pacer = self.pacer.next_deadline();
                    let retry = self.punch_next_retry;
                    return [Some(deadline), turn, probe, spray, pacer, retry]
                        .into_iter()
                        .flatten()
                        .min()
                        .unwrap_or(deadline);
                }
                Ok(Output::Transmit(transmit)) => {
                    let bytes: Vec<u8> = transmit.contents.into();
                    self.send_transmit(transmit.source, transmit.destination, bytes)
                        .await;
                }
                Ok(Output::Event(event)) => self.handle_rtc_event(event),
                Err(error) => {
                    self.fail(format!("rtc error: {error}"));
                    return Instant::now() + Duration::from_secs(3600);
                }
            }
        }
    }

    /// Sends one datagram the session produced, pacing large host video RTP
    /// packets so keyframe bursts do not overflow shallow residential uplinks.
    async fn send_transmit(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        payload: Vec<u8>,
    ) {
        if !Message::is_stun(&payload) {
            // Only the selected pair carries DTLS, SRTP and SCTP.
            self.observe_path(source, destination);
        }

        if self.config.role == Role::Host && !self.bwe_enabled && pacer::should_pace(&payload) {
            let now = Instant::now();
            if !self.pacer.try_consume_immediate(payload.len(), now) {
                self.pacer.enqueue(source, destination, payload);
                return;
            }
        }

        self.send_datagram_now(source, destination, &payload).await;
    }

    /// Flushes any paced datagrams whose token budget is ready.
    async fn flush_pacer(&mut self) {
        let now = Instant::now();
        while let Some(datagram) = self.pacer.poll_ready(now) {
            self.send_datagram_now(datagram.source, datagram.destination, &datagram.payload)
                .await;
        }
    }

    /// Writes one datagram immediately to the right socket (or TURN relay).
    ///
    /// A transmit whose source is our relay address is wrapped for the TURN
    /// allocation (ChannelData or a Send indication) and goes to the server;
    /// anything else goes straight to its destination.
    async fn send_datagram_now(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        payload: &[u8],
    ) {
        if let Some(turn) = self.turn.as_mut() {
            if turn.relayed() == Some(source) {
                match turn.encapsulate(destination, payload) {
                    Some(frame) => {
                        let server = turn.server();
                        if let Err(error) = self.socket_for(server).send_to(&frame, server).await {
                            debug!("send to turn server {server} failed: {error}");
                        }
                    }
                    None => {
                        // Peer reflexive candidates show up without a trickled
                        // candidate; open the path on first use.
                        if !turn.knows_peer(destination) {
                            turn.add_peer(destination, Instant::now());
                        }
                        debug!("relay path to {destination} not ready, dropping datagram");
                    }
                }
                return;
            }
        }

        if let Some((ref sock, addr)) = self.punch_socket {
            if source == addr {
                if let Err(error) = sock.send_to(payload, destination).await {
                    debug!("punched socket send to {destination} failed: {error}");
                }
                return;
            }
        }

        if let Err(error) = self
            .socket_for(destination)
            .send_to(payload, destination)
            .await
        {
            debug!("send to {destination} failed: {error}");
        }
    }

    /// The socket that can reach `destination`: the IPv6 one when there is
    /// one and the destination is IPv6, the bound socket otherwise.
    fn socket_for(&self, destination: SocketAddr) -> &UdpSocket {
        match &self.socket6 {
            Some((socket, _)) if destination.is_ipv6() => socket,
            _ => &self.socket,
        }
    }

    /// Sends the datagrams the TURN client queued.
    async fn flush_turn(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let server = turn.server();
        let socket = match &self.socket6 {
            Some((socket, _)) if server.is_ipv6() => socket,
            _ => &self.socket,
        };
        while let Some(datagram) = turn.poll_transmit() {
            if let Err(error) = socket.send_to(&datagram, server).await {
                debug!("send to turn server {server} failed: {error}");
            }
        }
    }

    /// Reacts to TURN client events. Returns true when the session changed.
    fn handle_turn_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.turn.as_mut().and_then(|turn| turn.poll_event()) {
            match event {
                TurnEvent::Allocated(relayed) => {
                    info!("turn allocation ready at {relayed}");
                    let server = self.turn.as_ref().map(|turn| turn.server());
                    let base = SocketAddr::new(
                        server
                            .and_then(|server| route_ip(server.ip()))
                            .unwrap_or(self.local_addr.ip()),
                        self.local_addr.port(),
                    );
                    match Candidate::relayed(relayed, base, "udp") {
                        Ok(candidate) => self.add_local_candidate(candidate),
                        Err(error) => warn!("relayed candidate rejected: {error}"),
                    }
                    changed = true;
                }
                TurnEvent::Failed { code, reason } => {
                    warn!("turn allocation failed: {code} {reason}");
                    if self.config.relay_only {
                        self.fail(format!("relay unavailable: {code} {reason}"));
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Records the pair media flows on and reports a path change.
    fn observe_path(&mut self, local: SocketAddr, remote: SocketAddr) {
        if self.path == Some((local, remote)) {
            return;
        }
        self.path = Some((local, remote));
        self.relayed = self.is_relayed(local, remote);
        info!(
            "media path {local} -> {remote} ({})",
            if self.relayed { "relayed" } else { "direct" }
        );
        self.apply_bitrate_ceiling();
        self.emit_path();
        if self.relayed {
            self.maybe_start_punch();
        } else {
            self.punch_cancel = None;
            self.punch_next_retry = None;
            self.host_spray = None;
        }
    }

    /// True when either end of the pair is a relay candidate.
    fn is_relayed(&self, local: SocketAddr, remote: SocketAddr) -> bool {
        let local_relay = self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.relayed() == Some(local));
        local_relay || self.remote_relays.contains(&remote)
    }

    fn emit_path(&self) {
        (self.config.sink)(Event::PathUpdate {
            rtt_ms: self.rtt_ms,
            relayed: self.relayed,
            estimated_bitrate: self.estimated_bitrate,
        });
    }

    /// Handles one datagram that arrived on the socket bound to `local`.
    fn handle_datagram(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        destination: Option<IpAddr>,
        local: SocketAddr,
    ) {
        let now = Instant::now();
        if self.handle_reflexive_response(datagram, from) {
            return;
        }
        if self.handle_punch_packet(datagram, from) {
            return;
        }

        // Traffic from the TURN server is a transaction response or a peer
        // datagram wrapped in ChannelData or a Data indication. Unwrapped peer
        // datagrams arrived on our relay address, from the peer's address.
        if let Some(turn) = self.turn.as_mut() {
            if turn.server() == from {
                match turn.handle_datagram(datagram, now) {
                    TurnReceived::Peer { peer, payload } => {
                        if let Some(relayed) = turn.relayed() {
                            self.feed(&payload, peer, relayed);
                        }
                        return;
                    }
                    TurnReceived::Consumed => return,
                    TurnReceived::NotTurn => {}
                }
            }
        }

        match self.resolve_destination(from, destination, local) {
            Some(destination) => self.feed(datagram, from, destination),
            None => debug!("dropping datagram from {from}: unknown destination"),
        }
    }

    /// Picks the local address a datagram arrived on.
    ///
    /// ICE drops checks that did not arrive on a local candidate, so an
    /// unspecified address is never handed to the session: when the platform
    /// does not report it, the bound address or the route to the sender is used.
    fn resolve_destination(
        &mut self,
        from: SocketAddr,
        reported: Option<IpAddr>,
        local: SocketAddr,
    ) -> Option<SocketAddr> {
        let port = local.port();
        if let Some(ip) = reported.filter(|ip| !ip.is_unspecified()) {
            return Some(SocketAddr::new(ip, port));
        }
        if !local.ip().is_unspecified() {
            return Some(local);
        }
        if let Some(ip) = self.routes.get(&from) {
            return Some(SocketAddr::new(*ip, port));
        }
        let ip = route_ip(from.ip())?;
        if self.routes.len() > 256 {
            self.routes.clear();
        }
        self.routes.insert(from, ip);
        Some(SocketAddr::new(ip, port))
    }

    /// Hands one datagram to the session.
    fn feed(&mut self, datagram: &[u8], from: SocketAddr, destination: SocketAddr) {
        let Ok(receive) = Receive::new(Protocol::Udp, from, destination, datagram) else {
            return;
        };
        self.rtc
            .handle_input(Input::Receive(Instant::now(), receive))
            .ok();
    }

    async fn gather_candidates(&mut self) {
        let port = self.local_addr.port();
        let now = Instant::now();

        if self.config.relay_only {
            if self.config.turn.is_none() {
                warn!("relay only mode without a TURN server: no candidates at all");
            }
        } else {
            // A socket bound to a concrete address can only be reached through
            // that address, so advertising every interface would create dead
            // candidates. A socket bound to `0.0.0.0` cannot send to an IPv6
            // address at all, so the candidate family follows the socket:
            // otherwise every IPv6 host candidate is a guaranteed dead pair.
            let interfaces = if self.local_addr.ip().is_unspecified() {
                local_ips()
            } else {
                vec![self.local_addr.ip()]
            };
            let port6 = self.socket6.as_ref().map(|(_, address)| address.port());
            for ip in interfaces {
                // IPv6 addresses go through the IPv6 socket, and only when it
                // exists; loopback and site addresses never leave the LAN, so
                // only global ones are worth a candidate there.
                let port = match ip {
                    _ if ip.is_ipv4() == self.local_addr.is_ipv4() => port,
                    IpAddr::V6(v6) if is_global_v6(v6) => match port6 {
                        Some(port) => port,
                        None => continue,
                    },
                    _ => continue,
                };
                if let Ok(candidate) = Candidate::host(SocketAddr::new(ip, port), "udp") {
                    self.add_local_candidate(candidate);
                }
            }

            if self.config.port_mapping {
                self.start_port_mapping();
            }

            if let Some(server) = self.config.stun {
                let builder = Builder::new(method::BINDING, class::REQUEST);
                let transaction_id = builder.id();
                let request = builder.build(None, true);
                if let Err(error) = self.socket.send_to(&request, server).await {
                    warn!("stun request to {server} failed: {error}");
                }
                self.reflexive = Some(ReflexiveProbe {
                    server,
                    transaction_id,
                    request,
                    next_send: now + REFLEXIVE_RTO,
                    rto: REFLEXIVE_RTO,
                    transmissions: 1,
                });
            }
        }

        if let Some(turn_config) = self.config.turn.clone() {
            self.turn = Some(TurnClient::new(
                turn_config.server,
                turn_config.username,
                turn_config.password,
                now,
            ));
            self.flush_turn().await;
        }
    }

    /// Asks the router for a port mapping in the background; the candidate is
    /// trickled when it arrives. The lease lives until the driver is dropped.
    #[cfg(feature = "port-mapping")]
    fn start_port_mapping(&mut self) {
        let interfaces = mapping_interfaces(self.config.stun);
        if interfaces.is_empty() {
            return;
        }
        let (result, mapping) = tokio::sync::oneshot::channel();
        let (lease, released) = tokio::sync::oneshot::channel();
        self.mapping = Some(mapping);
        self._mapping_lease = Some(lease);
        tokio::spawn(crate::portmap::hold(
            interfaces,
            self.local_addr.port(),
            result,
            released,
        ));
    }

    #[cfg(not(feature = "port-mapping"))]
    fn start_port_mapping(&mut self) {
        warn!("port mapping requested, but waystone-rtc was built without it");
    }

    /// Consumes the answer to the reflexive probe. Returns true when it was one.
    fn handle_reflexive_response(&mut self, datagram: &[u8], from: SocketAddr) -> bool {
        let Some(probe) = self.reflexive.as_ref() else {
            return false;
        };
        if from != probe.server || !Message::is_stun(datagram) {
            return false;
        }
        let Ok(message) = Message::parse(datagram) else {
            return false;
        };
        if message.transaction_id != probe.transaction_id {
            return false;
        }
        self.reflexive = None;
        let Some(mapped) = message.xor_mapped_address.or(message.mapped_address) else {
            warn!("stun reply from {from} without a mapped address");
            return true;
        };
        let base = SocketAddr::new(
            route_ip(from.ip()).unwrap_or(self.local_addr.ip()),
            self.local_addr.port(),
        );
        match Candidate::server_reflexive(mapped, base, "udp") {
            Ok(candidate) => self.add_local_candidate(candidate),
            Err(error) => warn!("srflx candidate rejected: {error}"),
        }
        true
    }

    fn add_local_candidate(&mut self, candidate: Candidate) {
        let text = candidate.to_sdp_string();
        if self.rtc.add_local_candidate(candidate).is_some() {
            (self.config.sink)(Event::LocalCandidate(text));
        }
    }

    /// Records a remote candidate: relay candidates mark relayed paths, and
    /// every address gets a TURN permission so the relay can reach it.
    fn note_remote_candidate(&mut self, candidate: &Candidate) {
        let address = candidate.addr();
        if candidate.kind() == CandidateKind::Relayed {
            self.remote_relays.insert(address);
        } else if candidate.kind() == CandidateKind::ServerReflexive
            && punch::is_punchable_ipv4(address)
            && self.remote_srflx.is_none()
        {
            self.remote_srflx = Some(address);
            self.maybe_start_punch();
        }
        if let Some(turn) = self.turn.as_mut() {
            turn.add_peer(address, Instant::now());
        }
    }

    /// Starts NAT3<->NAT4 birthday hole punching once the session is connected
    /// over a TURN relay and both ends have exchanged `srflx` IPv4s.
    fn maybe_start_punch(&mut self) {
        if self.punch_attempts >= punch::MAX_PUNCH_ATTEMPTS
            || self.host_spray.is_some()
            || self.punch_rx.is_some()
            || self.punch_next_retry.is_some()
            || self.config.relay_only
            || !self.connected
            || !self.relayed
            || self.turn.is_none()
        {
            return;
        }
        let Some(peer_srflx) = self.remote_srflx else {
            return;
        };
        self.punch_attempts += 1;
        let attempt = self.punch_attempts;
        match self.config.role {
            Role::Host => {
                info!(
                    "relayed session: spraying {} NAT3 pinholes toward {} (attempt {}/{})",
                    punch::HOST_SPRAY_TOTAL,
                    peer_srflx.ip(),
                    attempt,
                    punch::MAX_PUNCH_ATTEMPTS
                );
                let now = Instant::now();
                let mut spray = HostSpray::new(peer_srflx.ip(), now);
                spray.step(&self.socket, now);
                if !spray.is_done() {
                    self.host_spray = Some(spray);
                } else {
                    self.schedule_punch_retry(now);
                }
            }
            Role::Controller => {
                info!(
                    "relayed session: starting birthday punch burst toward {peer_srflx} (attempt {}/{})",
                    attempt,
                    punch::MAX_PUNCH_ATTEMPTS
                );
                let local_ip = route_ip(peer_srflx.ip()).unwrap_or(self.local_addr.ip());
                let session_token = self.punch_token;
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                self.punch_rx = Some(result_rx);
                self.punch_cancel = Some(cancel_tx);
                tokio::spawn(async move {
                    let result =
                        punch::run_controller_burst(peer_srflx, local_ip, session_token, cancel_rx)
                            .await;
                    let _ = result_tx.send(result);
                });
            }
        }
    }

    fn schedule_punch_retry(&mut self, now: Instant) {
        if !self.relayed
            || !self.connected
            || self.punch_attempts >= punch::MAX_PUNCH_ATTEMPTS
            || self.punch_next_retry.is_some()
        {
            return;
        }
        let idx =
            (self.punch_attempts.saturating_sub(1) as usize).min(punch::RETRY_DELAYS.len() - 1);
        let delay = punch::RETRY_DELAYS[idx];
        debug!(
            "birthday punch attempt {} did not upgrade yet; retrying in {:?}",
            self.punch_attempts, delay
        );
        self.punch_next_retry = Some(now + delay);
    }

    /// Consumes birthday-punch control packets so they never enter `str0m`.
    fn handle_punch_packet(&mut self, datagram: &[u8], from: SocketAddr) -> bool {
        let Some(packet) = punch::decode(datagram) else {
            return false;
        };
        if let PunchPacket::Hit(token) = packet {
            let authorized = self.config.role == Role::Host
                && punch::is_punchable_ipv4(from)
                && (self.punch_token.is_some_and(|expected| expected == token)
                    || self
                        .remote_srflx
                        .is_some_and(|srflx| srflx.ip() == from.ip()));
            if authorized {
                self.host_spray = None;
                self.punch_next_retry = None;
                let ack = punch::encode_ack(token);
                for _ in 0..3 {
                    let _ = self.socket.try_send_to(&ack, from);
                }
                info!("birthday punch hit from {from}, promoting direct candidate");
                if let Ok(candidate) = Candidate::host(from, "udp") {
                    self.rtc.add_remote_candidate(candidate);
                }
            }
        }
        true
    }

    /// Registers the controller's winning punched socket with `str0m` so ICE
    /// checks flow through the open pinhole and upgrade media off the relay.
    fn install_punched_socket(&mut self, punched: PunchedSocket) {
        let local_addr = punched.local_addr;
        self.punch_socket = Some((punched.socket, local_addr));
        match Candidate::host(local_addr, "udp") {
            Ok(candidate) => {
                info!("promoting punched socket {local_addr} for direct ICE");
                self.add_local_candidate(candidate);
            }
            Err(error) => warn!("punched candidate rejected: {error}"),
        }
    }

    /// Notes the candidates embedded in a remote SDP.
    fn note_sdp_candidates(&mut self, sdp: &str) {
        for line in sdp.lines() {
            let Some(text) = line.trim().strip_prefix("a=") else {
                continue;
            };
            if !text.starts_with("candidate:") {
                continue;
            }
            if let Ok(candidate) = Candidate::from_sdp_string(text) {
                self.note_remote_candidate(&candidate);
            }
        }
    }
    fn declare_local_media(&mut self) {
        if self.config.role != Role::Controller {
            return;
        }
        let audio = self
            .config
            .session
            .map(|session| session.audio)
            .unwrap_or(true);

        let mut changes = self.rtc.sdp_api();
        let video = changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = audio
            .then(|| changes.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None));
        self.video_mid = Some(video);
        self.audio_mid = audio_mid;

        self.fast_channel = Some(changes.add_channel_with_config(ChannelConfig {
            label: CHANNEL_INPUT_FAST.to_string(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..Default::default()
        }));
        self.reliable_channel = Some(changes.add_channel_with_config(ChannelConfig {
            label: CHANNEL_INPUT_RELIABLE.to_string(),
            ordered: true,
            ..Default::default()
        }));

        // The controller is the offering side, so the first negotiation starts as
        // soon as the media sections and data channels are declared.
        if let Some((offer, pending)) = changes.apply() {
            let sdp = offer.to_sdp_string();
            self.punch_token = punch::derive_session_token(&sdp);
            self.pending_offer = Some(pending);
            (self.config.sink)(Event::LocalOffer(sdp));
        }
    }

    fn handle_rtc_event(&mut self, event: RtcEvent) {
        match event {
            RtcEvent::Connected => {
                self.connected = true;
                (self.config.sink)(Event::Connected);
                self.resolve_media();
                self.maybe_start_punch();
            }
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                (self.config.sink)(Event::Disconnected)
            }
            RtcEvent::MediaData(data) => {
                let keyframe = data.is_keyframe();
                if Some(data.mid) == self.video_mid {
                    if !data.contiguous && !keyframe {
                        self.video_awaiting_keyframe = true;
                        self.request_video_pli(data.mid);
                        return;
                    }
                    if self.video_awaiting_keyframe {
                        if keyframe {
                            self.video_awaiting_keyframe = false;
                        } else {
                            self.request_video_pli(data.mid);
                            return;
                        }
                    }
                    (self.config.sink)(Event::VideoFrame {
                        data: data.data.to_vec(),
                        keyframe,
                        rtp_time: data.time.numer(),
                    });
                } else if Some(data.mid) == self.audio_mid {
                    (self.config.sink)(Event::AudioFrame {
                        data: data.data.to_vec(),
                        rtp_time: data.time.numer(),
                    });
                }
            }
            RtcEvent::MediaAdded(added) => {
                // The host side learns the mids from the controller offer through
                // this event; the controller already created them locally.
                let is_video = added.kind == MediaKind::Video;
                if is_video && self.video_mid.is_none() {
                    self.video_mid = Some(added.mid);
                } else if !is_video && self.audio_mid.is_none() {
                    self.audio_mid = Some(added.mid);
                }
                // Each new media section re-announces the negotiated set so the
                // caller always sees the complete picture.
                if self.connected {
                    self.resolve_media();
                }
            }
            RtcEvent::KeyframeRequest(request) => {
                // Only the host side produces frames, so only it acts on this.
                if self.config.role == Role::Host {
                    let _ = request.mid;
                    (self.config.sink)(Event::KeyframeRequest);
                }
            }
            RtcEvent::ChannelOpen(id, label) => {
                (self.config.sink)(Event::ChannelOpen(label.clone()));
                if label == CHANNEL_INPUT_FAST {
                    self.fast_channel = Some(id);
                } else if label == CHANNEL_INPUT_RELIABLE {
                    self.reliable_channel = Some(id);
                }
            }
            RtcEvent::ChannelData(data) => {
                let reliable = self.reliable_channel == Some(data.id);
                (self.config.sink)(Event::Input {
                    reliable,
                    data: data.data,
                });
            }
            RtcEvent::PeerStats(stats) => {
                self.rtt_ms = stats.rtt.map(|rtt| rtt.as_millis() as u32);
                self.estimated_bitrate = stats.bwe_tx.map(|bitrate| bitrate.as_u64() as u32);
                match stats.selected_candidate_pair.as_ref() {
                    // The selected pair reports the local base address, which
                    // for a relay candidate is the relayed address itself.
                    Some(pair) if self.path != Some((pair.local.addr, pair.remote.addr)) => {
                        // Reports the change itself.
                        self.observe_path(pair.local.addr, pair.remote.addr);
                    }
                    _ => self.emit_path(),
                }
            }
            other => debug!("unhandled rtc event: {other:?}"),
        }
    }

    /// Updates the outgoing media pacer and, when `WAYSTONE_BWE` is enabled,
    /// the str0m bandwidth estimator target.
    fn apply_bitrate_ceiling(&mut self) {
        let relay_ceiling = self
            .config
            .turn
            .as_ref()
            .map_or(0, |turn| turn.relay_bitrate_ceiling);
        self.pacer
            .configure(self.video_bitrate, self.relayed, relay_ceiling);

        if !self.bwe_enabled {
            return;
        }
        let ceiling = match &self.config.turn {
            Some(turn) if self.relayed => turn.relay_bitrate_ceiling,
            _ => u32::MAX,
        };
        let target = self.video_bitrate.min(ceiling);
        let minimum = str0m::bwe::Bitrate::bps(target.min(600_000) as u64);
        let desired = str0m::bwe::Bitrate::bps(target as u64);
        self.rtc.bwe().set_bitrate(minimum, desired);
    }

    fn request_video_pli(&mut self, mid: Mid) {
        let now = Instant::now();
        if self
            .last_pli
            .is_some_and(|at| now.duration_since(at) < PLI_MIN_INTERVAL)
        {
            return;
        }
        if let Some(mut writer) = self.rtc.writer(mid) {
            match writer.request_keyframe(None, KeyframeRequestKind::Pli) {
                Ok(()) => {
                    self.last_pli = Some(now);
                    debug!("requested video PLI after discontinuity");
                }
                Err(error) => debug!("video PLI request failed: {error}"),
            }
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::AcceptOffer(ref sdp) | Command::AcceptAnswer(ref sdp) => {
                self.note_sdp_candidates(sdp);
                self.handle_session_command(command);
            }
            other => self.handle_session_command(other),
        }
    }

    fn handle_session_command(&mut self, command: Command) {
        match command {
            Command::Offer => {
                if let Some((offer, pending)) = self.rtc.sdp_api().apply() {
                    let sdp = offer.to_sdp_string();
                    self.punch_token = punch::derive_session_token(&sdp);
                    self.pending_offer = Some(pending);
                    (self.config.sink)(Event::LocalOffer(sdp));
                }
            }
            Command::AcceptOffer(sdp) => {
                self.punch_token = punch::derive_session_token(&sdp);
                match SdpOffer::from_sdp_string(&sdp) {
                    Ok(offer) => match self.rtc.sdp_api().accept_offer(offer) {
                        Ok(answer) => {
                            (self.config.sink)(Event::LocalAnswer(answer.to_sdp_string()));
                        }
                        Err(error) => self.fail(format!("accept offer: {error}")),
                    },
                    Err(error) => self.fail(format!("parse offer: {error}")),
                }
            }
            Command::AcceptAnswer(sdp) => match SdpAnswer::from_sdp_string(&sdp) {
                Ok(answer) => match self.pending_offer.take() {
                    Some(pending) => match self.rtc.sdp_api().accept_answer(pending, answer) {
                        Ok(()) => self.resolve_media(),
                        Err(error) => self.fail(format!("accept answer: {error}")),
                    },
                    None => self.fail("answer without a pending offer".into()),
                },
                Err(error) => self.fail(format!("parse answer: {error}")),
            },
            Command::RemoteCandidate(text) => {
                // Accept both `candidate:...` and the `a=candidate:...` SDP form.
                let text = text.trim();
                let text = text.strip_prefix("a=").unwrap_or(text);
                match Candidate::from_sdp_string(text) {
                    Ok(candidate) => {
                        self.note_remote_candidate(&candidate);
                        self.rtc.add_remote_candidate(candidate);
                    }
                    Err(error) => warn!("bad remote candidate: {error}"),
                }
            }
            Command::SetVideoBitrate(bits) => {
                self.video_bitrate = bits;
                self.apply_bitrate_ceiling();
            }
            Command::Shutdown => {
                self.closed = true;
            }
        }
    }

    fn handle_media(&mut self, media: Media) {
        match media {
            Media::Audio { data, rtp_time } => self.write_audio(data, rtp_time),
            Media::Input { reliable, data } => self.write_input(reliable, &data),
        }
    }

    fn write_queued_video(&mut self) {
        if self.video.dropped.swap(false, Ordering::Relaxed) {
            debug!("video queue overflowed, oldest frames dropped");
            (self.config.sink)(Event::FrameDropped);
        }
        for (data, rtp_time) in self.video.take() {
            self.write_video(data, rtp_time);
        }
    }

    fn write_video(&mut self, data: Vec<u8>, rtp_time: u64) {
        let Some(mid) = self.video_mid else {
            (self.config.sink)(Event::FrameDropped);
            return;
        };
        let Some(pt) = self.payload_type(mid, str0m_codec(self.config.video_codec)) else {
            self.video_drop_logged += 1;
            if self.video_drop_logged <= 3 {
                warn!(
                    "video frame dropped: {:?} is not negotiated for {mid:?}",
                    self.config.video_codec
                );
            }
            (self.config.sink)(Event::FrameDropped);
            return;
        };
        let time = MediaTime::new(rtp_time, Frequency::NINETY_KHZ);
        match self.rtc.writer(mid) {
            Some(writer) => {
                if let Err(error) = writer.write(pt, Instant::now(), time, data) {
                    debug!("video write failed: {error}");
                }
            }
            None => (self.config.sink)(Event::FrameDropped),
        }
    }

    fn write_audio(&mut self, data: Vec<u8>, rtp_time: u64) {
        let Some(mid) = self.audio_mid else { return };
        let Some(pt) = self.payload_type(mid, Codec::Opus) else {
            return;
        };
        let time = MediaTime::new(rtp_time, Frequency::FORTY_EIGHT_KHZ);
        if let Some(writer) = self.rtc.writer(mid) {
            if let Err(error) = writer.write(pt, Instant::now(), time, data) {
                debug!("audio write failed: {error}");
            }
        }
    }

    fn write_input(&mut self, reliable: bool, data: &[u8]) {
        let channel = if reliable {
            self.reliable_channel
        } else {
            self.fast_channel
        };
        let Some(id) = channel else { return };
        let Some(mut channel) = self.rtc.channel(id) else {
            return;
        };
        match channel.write(true, data) {
            Ok(true) => {}
            Ok(false) => debug!("input channel congested, dropping frame"),
            Err(error) => debug!("input write failed: {error}"),
        }
    }

    fn payload_type(&mut self, mid: Mid, codec: Codec) -> Option<Pt> {
        let writer = self.rtc.writer(mid)?;
        let pt = writer
            .payload_params()
            .find(|params| params.spec().codec == codec)
            .map(|params| params.pt());
        pt
    }

    /// Emits [`Event::MediaReady`] once both sides agree on the media sections.
    fn resolve_media(&mut self) {
        if self.video_mid.is_none() && self.audio_mid.is_none() {
            return;
        }
        (self.config.sink)(Event::MediaReady {
            video: self.video_mid,
            audio: self.audio_mid,
        });
    }

    fn fail(&mut self, reason: String) {
        warn!("session failed: {reason}");
        (self.config.sink)(Event::Closed(reason));
        self.closed = true;
    }
}

/// Enumerates the usable local interface addresses.
///
/// Every address is advertised, which is what makes a session work when the
/// machine has several interfaces (Wi-Fi, Ethernet, a VPN tunnel). The probe
/// socket trick is not enough: with a VPN it can select a tunnel address the peer
/// cannot reach, and when the probe fails it yields nothing at all.
fn local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            // Tunnel interfaces carry VPN and packet filter addresses. A peer
            // usually cannot reach them, and when the tunnel is a transparent
            // proxy the packets are silently dropped, so they are skipped; the
            // WiFi, Ethernet or loopback address provides the working path.
            if is_tunnel(&interface.name) {
                debug!("skipping tunnel interface {}", interface.name);
                continue;
            }
            let ip = interface.ip();
            if ip.is_loopback() || ip.is_unspecified() {
                continue;
            }
            // Link local addresses are not routable for a viewer on another host
            // and only produce dead candidates.
            if match ip {
                IpAddr::V4(v4) => v4.is_link_local(),
                IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
            } {
                continue;
            }
            // Point to point style interfaces can carry the network or broadcast
            // address of their subnet (a VM bridge on this machine does). Those
            // addresses cannot be a unicast destination at all, so every
            // candidate built from them is dead on arrival.
            if !is_sendable(ip) {
                debug!("skipping unreachable address {ip} on {}", interface.name);
                continue;
            }
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }
    // Loopback is advertised as well: two engines on one machine (and the iOS
    // simulator, which shares the host network namespace) can then connect
    // without a routable interface in common. A remote peer simply fails this
    // pair during connectivity checks.
    let loopback: IpAddr = if ips.iter().any(|ip| ip.is_ipv4()) {
        IpAddr::from([127, 0, 0, 1])
    } else {
        "::1".parse().expect("static loopback")
    };
    if !ips.contains(&loopback) {
        ips.push(loopback);
    }
    ips
}

fn is_tunnel(name: &str) -> bool {
    const PREFIXES: [&str; 8] = ["utun", "ipsec", "tun", "tap", "ppp", "awdl", "llw", "gif"];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// True when a packet to `ip` can leave this machine.
///
/// The kernel reports addresses it cannot route to (the network or broadcast
/// address of a bridge, for instance); a zero length datagram is the cheapest
/// way to ask, and a candidate that fails here would only produce failed
/// connectivity checks.
fn is_sendable(ip: IpAddr) -> bool {
    let bind: SocketAddr = if ip.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let Ok(socket) = std::net::UdpSocket::bind(bind) else {
        return false;
    };
    socket.send_to(&[], SocketAddr::new(ip, 9)).is_ok()
}

/// The local address the kernel would use to reach `remote`.
///
/// Connecting a UDP socket sends nothing; it only performs the route lookup.
fn route_ip(remote: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if remote.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = std::net::UdpSocket::bind(bind).ok()?;
    socket.connect(SocketAddr::new(remote, 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Private IPv4 addresses a router could forward to, the one routing to
/// `stun` (or any public address) first.
///
/// The route alone is not enough: with a VPN or a proxy's TUN device the
/// default route points into the tunnel, whose address no router knows.
#[cfg(feature = "port-mapping")]
pub(crate) fn mapping_interfaces(stun: Option<SocketAddr>) -> Vec<std::net::Ipv4Addr> {
    let private = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) if v4.is_private() => Some(*v4),
        _ => None,
    };
    let probe = stun.map_or(IpAddr::from([223, 5, 5, 5]), |server| server.ip());
    let mut interfaces: Vec<_> = route_ip(probe)
        .as_ref()
        .and_then(private)
        .into_iter()
        .collect();
    for ip in local_ips().iter().filter_map(private) {
        if !interfaces.contains(&ip) {
            interfaces.push(ip);
        }
    }
    interfaces
}

/// Opens the IPv6-only socket that complements the default IPv4 one.
///
/// `IPV6_V6ONLY` is set explicitly because the default differs per platform
/// (off on Linux and macOS, on for Windows): a dual-stack socket would also
/// receive IPv4 traffic as mapped addresses nobody advertised.
fn bind_ipv6() -> std::io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_only_v6(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from(([0u16; 8], 0)).into())?;
    let socket = UdpSocket::from_std(socket.into())?;
    crate::net::configure_udp_buffers(&socket);
    if let Err(error) = crate::net::enable_destination_info(&socket) {
        debug!("cannot report IPv6 datagram destinations: {error}");
    }
    Ok(socket)
}

/// Global unicast IPv6 (`2000::/3`), the only kind a remote peer can reach.
fn is_global_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xe000) == 0x2000
}

/// Receives from the IPv6 socket, or waits forever when there is none.
async fn recv_optional(
    socket: Option<&(UdpSocket, SocketAddr)>,
    buffer: &mut [u8],
) -> std::io::Result<(crate::net::Datagram, SocketAddr)> {
    match socket {
        Some((socket, local)) => {
            let datagram = crate::net::recv_with_destination(socket, buffer).await?;
            Ok((datagram, *local))
        }
        None => std::future::pending().await,
    }
}

/// Waits for the controller birthday punch result, or forever when none is pending.
async fn wait_punch(
    pending: &mut Option<tokio::sync::oneshot::Receiver<Option<PunchedSocket>>>,
) -> Option<PunchedSocket> {
    match pending {
        Some(receiver) => receiver.await.ok().flatten(),
        None => std::future::pending().await,
    }
}

/// Waits for the port mapping result, or forever when none is pending.
async fn wait_mapping(
    pending: &mut Option<tokio::sync::oneshot::Receiver<Option<(SocketAddr, SocketAddr)>>>,
) -> Option<(SocketAddr, SocketAddr)> {
    match pending {
        Some(receiver) => receiver.await.ok().flatten(),
        None => std::future::pending().await,
    }
}

/// Read errors caused by an earlier send rather than a broken socket.
fn is_transient(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::Interrupted
    )
}

/// Helper used by tests and the FFI layer to build a sink from a closure.
pub fn sink<F>(f: F) -> EventSink
where
    F: Fn(Event) + Send + Sync + 'static,
{
    std::sync::Arc::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_addresses_are_not_sendable() {
        // A VM bridge on macOS reports the network address of its subnet.
        assert!(!is_sendable("192.168.215.0".parse().unwrap()));
        assert!(is_sendable("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn loopback_routes_to_loopback() {
        assert_eq!(
            route_ip("127.0.0.1".parse().unwrap()),
            Some("127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn only_global_ipv6_is_advertised() {
        assert!(is_global_v6("2408:8207:1850::1".parse().unwrap()));
        assert!(!is_global_v6("fd00::1".parse().unwrap()));
        assert!(!is_global_v6("fe80::1".parse().unwrap()));
        assert!(!is_global_v6("::1".parse().unwrap()));
    }

    #[test]
    fn tunnel_interfaces_are_skipped() {
        assert!(is_tunnel("utun4"));
        assert!(is_tunnel("awdl0"));
        assert!(!is_tunnel("en0"));
        assert!(!is_tunnel("bridge100"));
    }
}
