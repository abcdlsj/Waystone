//! The single-owner driver around a `str0m::Rtc` session.
//!
//! `str0m` is sans-IO and has one hard rule: every mutation must be followed by
//! draining `poll_output` until `Output::Timeout`. To make that rule impossible to
//! break from the outside, exactly one task owns the [`Rtc`]. Other threads post
//! commands into a channel and receive events through a callback.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::format::Codec;
use str0m::media::Frequency;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{
    Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, Input, Output, Rtc, RtcConfig,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use waystone_proto::Viewport;

use crate::stun::{self, class, method, Builder, Message};
use crate::turn::{self, Progress as TurnProgress, TurnAllocation};

/// Video codec to negotiate.
///
/// HEVC is the default and what every supported device decodes in hardware;
/// H.264 exists as a fallback because a few environments (notably the iOS
/// simulator and some old browsers) have no HEVC decoder at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    /// Parses the wire value used in the engine config.
    pub fn parse(value: &str) -> VideoCodec {
        match value.to_ascii_lowercase().as_str() {
            "h264" | "avc" => VideoCodec::H264,
            _ => VideoCodec::Hevc,
        }
    }

    /// The str0m codec this maps to.
    pub fn codec(self) -> Codec {
        match self {
            VideoCodec::Hevc => Codec::H265,
            VideoCodec::H264 => Codec::H264,
        }
    }

    /// Value used in the engine config and the FFI JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoCodec::Hevc => "h265",
            VideoCodec::H264 => "h264",
        }
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
    PathUpdate {
        rtt_ms: Option<u32>,
        relayed: bool,
        estimated_bitrate: Option<u32>,
    },
    /// The remote peer asked for a fresh keyframe.
    KeyframeRequest,
    /// A frame was dropped because the session is not ready yet.
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
    /// Video bitrate ceiling applied once the session falls back to relay.
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
    pub bind: Option<SocketAddr>,
    /// Codec the host encodes and the controller expects.
    pub video_codec: VideoCodec,
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
            video_codec: VideoCodec::Hevc,
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
}

/// Data channel label for high frequency pointer samples.
pub const CHANNEL_INPUT_FAST: &str = "input-fast";
/// Data channel label for reliable state edges.
pub const CHANNEL_INPUT_RELIABLE: &str = "input-reliable";

enum Command {
    Offer,
    AcceptOffer(String),
    AcceptAnswer(String),
    RemoteCandidate(String),
    SendVideo { data: Vec<u8>, rtp_time: u64 },
    SendAudio { data: Vec<u8>, rtp_time: u64 },
    SendInput { reliable: bool, data: Vec<u8> },
    SetVideoBitrate(u32),
    Shutdown,
}

/// Handle used by the rest of the application.
#[derive(Clone)]
pub struct Engine {
    tx: UnboundedSender<Command>,
    local_addr: SocketAddr,
}

impl Engine {
    /// Binds the transport socket and spawns the driver task.
    pub async fn start(config: EngineConfig) -> std::io::Result<Engine> {
        let bind = config.bind.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
        // A single socket serves every local interface, so host candidates only
        // differ in their IP address; the destination address of each datagram is
        // read from the ancillary data instead.
        let socket = UdpSocket::bind(bind).await?;
        if let Err(error) = crate::net::enable_destination_info(&socket) {
            warn!("cannot report datagram destinations: {error}");
        }
        let local_addr = socket.local_addr()?;
        let (tx, rx) = unbounded_channel();
        tokio::spawn(async move {
            let mut driver = Driver::new(config, socket, rx, local_addr);
            driver.run().await;
        });
        Ok(Engine { tx, local_addr })
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
    pub fn send_video(&self, data: Vec<u8>, rtp_time: u64) {
        let _ = self.tx.send(Command::SendVideo { data, rtp_time });
    }

    /// Writes one Opus packet with a 48 kHz timestamp.
    pub fn send_audio(&self, data: Vec<u8>, rtp_time: u64) {
        let _ = self.tx.send(Command::SendAudio { data, rtp_time });
    }

    /// Sends an encoded input frame on the matching data channel.
    pub fn send_input(&self, reliable: bool, data: Vec<u8>) {
        let _ = self.tx.send(Command::SendInput { reliable, data });
    }

    /// Caps the outgoing video bitrate; the encoder follows this value.
    pub fn set_video_bitrate(&self, bits_per_second: u32) {
        let _ = self.tx.send(Command::SetVideoBitrate(bits_per_second));
    }

    /// Tears the session down.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

struct Driver {
    config: EngineConfig,
    socket: UdpSocket,
    rx: UnboundedReceiver<Command>,
    rtc: Rtc,
    local_addr: SocketAddr,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    fast_channel: Option<ChannelId>,
    reliable_channel: Option<ChannelId>,
    /// Pending offer handle, kept between `apply()` and `accept_answer()`.
    pending_offer: Option<str0m::change::SdpPendingOffer>,
    turn: Option<TurnAllocation>,
    /// Peer reached through the TURN allocation.
    turn_peer: Option<SocketAddr>,
    relayed: bool,
    video_bitrate: u32,
    /// Dropped frames, so the warning is logged a bounded number of times.
    video_drop_logged: u64,
    connected: bool,
    closed: bool,
}

impl Driver {
    fn new(
        config: EngineConfig,
        socket: UdpSocket,
        rx: UnboundedReceiver<Command>,
        local_addr: SocketAddr,
    ) -> Self {
        // Both HEVC and H.264 are offered: HEVC is what every supported iPhone,
        // iPad and Mac decodes in hardware and is the preferred codec, while
        // H.264 is the universal fallback (the iOS simulator, for instance, has
        // no HEVC decoder at all). Opus is the only audio codec.
        let mut rtc_config = RtcConfig::new().clear_codecs();
        match config.video_codec {
            VideoCodec::Hevc => rtc_config.codec_config().enable_h265(true),
            VideoCodec::H264 => rtc_config.codec_config().enable_h264(true),
        }
        rtc_config.codec_config().enable_opus(true, false);
        // BWE is opt-in: without feedback the pacer would throttle the video to
        // the initial estimate, so the default is a fixed high ceiling with the
        // encoder doing the rate control.
        let rtc_config = rtc_config.set_stats_interval(Some(Duration::from_secs(1)));
        let rtc_config = match std::env::var_os("WAYSTONE_BWE") {
            Some(_) => rtc_config.enable_bwe(Some(str0m::bwe::Bitrate::mbps(2))),
            None => rtc_config,
        };

        let video_bitrate = config
            .session
            .map(|session| session.max_video_bitrate)
            .unwrap_or(8_000_000);

        Driver {
            rtc: rtc_config.build(Instant::now()),
            config,
            socket,
            rx,
            local_addr,
            video_mid: None,
            audio_mid: None,
            fast_channel: None,
            reliable_channel: None,
            pending_offer: None,
            turn: None,
            turn_peer: None,
            relayed: false,
            video_bitrate,
            video_drop_logged: 0,
            connected: false,
            closed: false,
        }
    }

    async fn run(&mut self) {
        self.gather_candidates().await;
        self.declare_local_media();

        loop {
            if self.closed {
                break;
            }

            let deadline = self.drain().await;
            let sleep = tokio::time::sleep_until(deadline.into());
            tokio::pin!(sleep);

            let mut buf = vec![0u8; 2048];
            // Deliberately not `biased`: with a video frame arriving every 16 ms
            // a biased select would drain commands first and starve the socket,
            // so TWCC feedback and NACKs would never be processed and the
            // receiver would keep waiting for a keyframe that already went out.
            tokio::select! {
                command = self.rx.recv() => match command {
                    Some(command) => self.handle_command(command),
                    None => break,
                },
                received = crate::net::recv_with_destination(&self.socket, &mut buf) => match received {
                    Ok(datagram) => {
                        let source = datagram.source;
                        let destination = datagram.destination.unwrap_or(self.local_addr.ip());
                        let payload = buf[..datagram.len].to_vec();
                        self.handle_datagram(&payload, source, destination).await;
                    }
                    Err(error) => {
                        warn!("socket read failed: {error}");
                        break;
                    }
                },
                _ = &mut sleep => {
                    self.rtc.handle_input(Input::Timeout(Instant::now())).ok();
                }
            }
        }

        (self.config.sink)(Event::Closed("engine stopped".into()));
        info!("engine task finished");
    }

    /// Drains every queued output and returns the next wake-up deadline.
    ///
    /// This performs the socket writes directly: the engine task is the only
    /// owner of the `Rtc`, so it may await here without breaking the invariant.
    async fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let turn_deadline = self.service_turn().await;
                    return turn_deadline.map(|t| t.min(deadline)).unwrap_or(deadline);
                }
                Ok(Output::Transmit(transmit)) => {
                    let bytes: Vec<u8> = transmit.contents.into();
                    self.send_datagram(transmit.destination, &bytes).await;
                }
                Ok(Output::Event(event)) => self.handle_rtc_event(event),
                Err(error) => {
                    self.fail(format!("rtc error: {error}"));
                    return Instant::now() + Duration::from_secs(3600);
                }
            }
        }
    }

    async fn send_datagram(&mut self, destination: SocketAddr, payload: &[u8]) {
        // Traffic to the relayed peer goes through the TURN allocation, which
        // needs channel binding to be confirmed first.
        if self.turn_peer == Some(destination) {
            let frame = match self.turn.as_ref() {
                Some(turn) => turn.frame_channel_data(destination, payload),
                None => None,
            };
            match frame {
                Some(frame) => {
                    if let Some(turn) = &self.turn {
                        let server = turn.server();
                        let _ = self.socket.send_to(&frame, server).await;
                    }
                }
                None => debug!("relay path not ready, dropping datagram"),
            }
            return;
        }

        if let Err(error) = self.socket.send_to(payload, destination).await {
            debug!("send to {destination} failed: {error}");
        }
    }

    /// Flushes TURN requests and drives the refresh timer.
    ///
    /// Returns the deadline the allocation wants to be polled at, if any.
    async fn service_turn(&mut self) -> Option<Instant> {
        let turn = self.turn.as_mut()?;
        if turn.needs_refresh(Instant::now()) {
            turn.refresh();
        }
        for datagram in turn.take_outgoing() {
            let server = turn.server();
            let _ = self.socket.send_to(&datagram, server).await;
        }
        Some(Instant::now() + turn::refresh_interval())
    }

    async fn handle_datagram(&mut self, datagram: &[u8], from: SocketAddr, destination: IpAddr) {
        // Traffic from the TURN server is either a STUN transaction response or
        // ChannelData wrapping a peer datagram.
        if self.turn.as_ref().map(|turn| turn.server()) == Some(from) {
            if let Some((_channel, payload)) = turn::parse_channel_data(datagram) {
                let peer = self.turn_peer;
                if let Some(peer) = peer {
                    self.feed(payload, peer, destination);
                }
            }
            if Message::is_stun(datagram) {
                if let Some(turn) = self.turn.as_mut() {
                    match turn.handle_message(datagram) {
                        TurnProgress::Allocated(relayed) => {
                            info!("turn allocation ready at {relayed}");
                            self.announce_relayed(relayed);
                        }
                        TurnProgress::Failed { code, reason } => {
                            warn!("turn rejected the request: {code} {reason}");
                        }
                        _ => {}
                    }
                }
            }
            let _ = self.service_turn().await;
        }

        self.feed(datagram, from, destination);
    }

    /// Hands one datagram to the session.
    ///
    /// `destination` matters: ICE drops connectivity checks that did not arrive on
    /// a local candidate, so a wildcard socket must report the real address.
    fn feed(&mut self, datagram: &[u8], from: SocketAddr, destination: IpAddr) {
        let destination = SocketAddr::new(destination, self.local_addr.port());
        let Ok(receive) = Receive::new(Protocol::Udp, from, destination, datagram) else {
            return;
        };
        self.rtc
            .handle_input(Input::Receive(Instant::now(), receive))
            .ok();
    }

    async fn gather_candidates(&mut self) {
        let port = self.local_addr.port();
        // A socket bound to a concrete address can only be reached through that
        // address, so advertising every interface would create dead candidates.
        // A socket bound to `0.0.0.0` cannot send to an IPv6 address at all, so
        // the candidate family follows the socket: otherwise every IPv6 host
        // candidate is a guaranteed dead pair.
        let ips = if self.local_addr.ip().is_unspecified() {
            local_ips()
                .into_iter()
                .filter(|ip| ip.is_ipv4() == self.local_addr.is_ipv4())
                .collect()
        } else {
            vec![self.local_addr.ip()]
        };
        for ip in ips {
            if let Ok(candidate) = Candidate::host(SocketAddr::new(ip, port), "udp") {
                self.add_local_candidate(candidate);
            }
        }

        if let Some(stun_server) = self.config.stun {
            match self.discover_reflexive(stun_server, port).await {
                Ok(mapped) => {
                    let base = SocketAddr::new(local_ip_for(stun_server.ip()), port);
                    match Candidate::server_reflexive(mapped, base, "udp") {
                        Ok(candidate) => self.add_local_candidate(candidate),
                        Err(error) => warn!("srflx candidate rejected: {error}"),
                    }
                }
                Err(error) => warn!("stun discovery failed: {error}"),
            }
        }

        if let Some(turn_config) = self.config.turn.clone() {
            self.turn = Some(TurnAllocation::start(
                turn_config.server,
                turn_config.username,
                turn_config.password,
            ));
            let _ = self.service_turn().await;
        }
    }

    fn announce_relayed(&mut self, relayed: SocketAddr) {
        let base = SocketAddr::new(local_ip_for(relayed.ip()), self.local_addr.port());
        match Candidate::relayed(relayed, base, "udp") {
            Ok(candidate) => self.add_local_candidate(candidate),
            Err(error) => warn!("relayed candidate rejected: {error}"),
        }
    }

    fn add_local_candidate(&mut self, candidate: Candidate) {
        let text = candidate.to_sdp_string();
        if self.rtc.add_local_candidate(candidate).is_some() {
            (self.config.sink)(Event::LocalCandidate(text));
        }
    }

    /// Sends one STUN binding request and returns the mapped address.
    async fn discover_reflexive(
        &mut self,
        server: SocketAddr,
        port: u16,
    ) -> Result<SocketAddr, std::io::Error> {
        let request = Builder::new(method::BINDING, class::REQUEST).build(None, true);
        self.socket.send_to(&request, server).await?;

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut buf = vec![0u8; 1024];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::other("stun timeout"));
            }
            let received = tokio::time::timeout(remaining, self.socket.recv_from(&mut buf)).await;
            let (len, from) = match received {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(std::io::Error::other("stun timeout")),
            };
            if from != server || !Message::is_stun(&buf[..len]) {
                // Anything else is legitimate traffic that arrived early; hand it
                // to the session so nothing is lost.
                self.feed(&buf[..len], from, self.local_addr.ip());
                continue;
            }
            let message = Message::parse(&buf[..len]).map_err(std::io::Error::other)?;
            let mapped = message
                .xor_mapped_address
                .or(message.mapped_address)
                .ok_or_else(|| std::io::Error::other("stun reply without mapped address"))?;
            let _ = port;
            return Ok(mapped);
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
            self.pending_offer = Some(pending);
            (self.config.sink)(Event::LocalOffer(offer.to_sdp_string()));
        }
    }

    fn handle_rtc_event(&mut self, event: RtcEvent) {
        match event {
            RtcEvent::Connected => {
                self.connected = true;
                (self.config.sink)(Event::Connected);
                self.resolve_media();
            }
            RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                (self.config.sink)(Event::Disconnected)
            }
            RtcEvent::MediaData(data) => {
                let keyframe = data.is_keyframe();
                if Some(data.mid) == self.video_mid {
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
                // The selected pair reports plain addresses, so the relay path is
                // detected by matching the local candidate against the allocation.
                let relayed = stats
                    .selected_candidate_pair
                    .as_ref()
                    .and_then(|pair| {
                        self.turn
                            .as_ref()
                            .map(|turn| turn.relayed() == Some(pair.local.addr))
                    })
                    .unwrap_or(self.relayed);
                if relayed != self.relayed {
                    self.apply_bitrate_ceiling(relayed);
                }
                (self.config.sink)(Event::PathUpdate {
                    rtt_ms: stats.rtt.map(|rtt| rtt.as_millis() as u32),
                    relayed,
                    estimated_bitrate: stats.bwe_tx.map(|bitrate| bitrate.as_u64() as u32),
                });
            }
            other => debug!("unhandled rtc event: {other:?}"),
        }
    }

    /// Clamps the video bitrate when the session falls back to the relay.
    fn apply_bitrate_ceiling(&mut self, relayed: bool) {
        self.relayed = relayed;
        let ceiling = match &self.config.turn {
            Some(turn) if relayed => turn.relay_bitrate_ceiling,
            _ => u32::MAX,
        };
        let target = self.video_bitrate.min(ceiling);
        let minimum = str0m::bwe::Bitrate::bps(target.min(600_000) as u64);
        let desired = str0m::bwe::Bitrate::bps(target as u64);
        self.rtc.bwe().set_bitrate(minimum, desired);
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Offer => {
                if let Some((offer, pending)) = self.rtc.sdp_api().apply() {
                    self.pending_offer = Some(pending);
                    (self.config.sink)(Event::LocalOffer(offer.to_sdp_string()));
                }
            }
            Command::AcceptOffer(sdp) => match SdpOffer::from_sdp_string(&sdp) {
                Ok(offer) => match self.rtc.sdp_api().accept_offer(offer) {
                    Ok(answer) => {
                        (self.config.sink)(Event::LocalAnswer(answer.to_sdp_string()));
                    }
                    Err(error) => self.fail(format!("accept offer: {error}")),
                },
                Err(error) => self.fail(format!("parse offer: {error}")),
            },
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
            Command::RemoteCandidate(text) => match Candidate::from_sdp_string(&text) {
                Ok(candidate) => {
                    let relayed = candidate.kind() == CandidateKind::Relayed;
                    let address = candidate.addr();
                    self.rtc.add_remote_candidate(candidate);
                    if relayed {
                        self.turn_peer = Some(address);
                        if let Some(turn) = self.turn.as_mut() {
                            turn.bind_channel(address);
                        }
                    }
                }
                Err(error) => warn!("bad remote candidate: {error}"),
            },
            Command::SendVideo { data, rtp_time } => self.write_video(data, rtp_time),
            Command::SendAudio { data, rtp_time } => self.write_audio(data, rtp_time),
            Command::SendInput { reliable, data } => self.write_input(reliable, &data),
            Command::SetVideoBitrate(bits) => {
                self.video_bitrate = bits;
                self.apply_bitrate_ceiling(self.relayed);
            }
            Command::Shutdown => {
                self.closed = true;
            }
        }
    }

    fn write_video(&mut self, data: Vec<u8>, rtp_time: u64) {
        let Some(mid) = self.video_mid else {
            (self.config.sink)(Event::FrameDropped);
            return;
        };
        let Some(pt) = self.payload_type(mid, self.config.video_codec.codec()) else {
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

fn local_ip_for(_remote: IpAddr) -> IpAddr {
    local_ips()
        .into_iter()
        .next()
        .unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

/// Helper used by tests and the FFI layer to build a sink from a closure.
pub fn sink<F>(f: F) -> EventSink
where
    F: Fn(Event) + Send + Sync + 'static,
{
    std::sync::Arc::new(f)
}

/// Maps a channel id back to its reliability class, for logging.
pub fn channel_map() -> HashMap<&'static str, bool> {
    HashMap::from([(CHANNEL_INPUT_FAST, false), (CHANNEL_INPUT_RELIABLE, true)])
}

/// Re-exported so callers can build STUN probes without depending on the module.
pub use stun::Builder as StunBuilder;

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
    fn tunnel_interfaces_are_skipped() {
        assert!(is_tunnel("utun4"));
        assert!(is_tunnel("awdl0"));
        assert!(!is_tunnel("en0"));
        assert!(!is_tunnel("bridge100"));
    }
}
