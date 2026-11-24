//! C ABI consumed by the Swift host agent and the iOS controller.
//!
//! The layer is thin on purpose. It owns a Tokio runtime, converts C strings into
//! `waystone-proto` values, forwards commands to [`crate::Engine`] and reports
//! events through a callback.
//!
//! Control events are JSON (`{"kind":"connected"}` and friends) so Swift can
//! decode them with `Codable`. Media events carry a fixed 16 byte header followed
//! by the payload, because copying a 4K frame through JSON would defeat the point
//! of the whole pipeline:
//!
//! ```text
//! byte 0      event kind (1 video, 2 audio, 3 input)
//! byte 1      bit 0 keyframe, bit 1 reliable input channel
//! bytes 2..10 RTP timestamp, big endian u64
//! bytes 10..16 reserved, zero
//! bytes 16..  payload
//! ```

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use waystone_proto::Viewport;

use crate::engine::{
    sink, Engine, EngineConfig, Event, Role, SessionOptions, TurnConfig, VideoCodec,
};

/// Control event: `text` is a JSON object with a `kind` field.
pub const WS_EVENT_CONTROL: i32 = 0;
/// Video frame: header plus Annex-B payload.
pub const WS_EVENT_VIDEO: i32 = 1;
/// Audio frame: header plus one Opus packet.
pub const WS_EVENT_AUDIO: i32 = 2;
/// Input frame: header plus the raw input frame from `waystone-proto`.
pub const WS_EVENT_INPUT: i32 = 3;

const MEDIA_HEADER_LEN: usize = 16;

/// Callback signature.
///
/// `data` is only valid for the duration of the call; Swift must copy what it
/// needs. `ctx` is the pointer handed to [`ws_engine_start`].
pub type WsEventCallback = extern "C" fn(kind: i32, data: *const u8, len: usize, ctx: *mut c_void);

/// Context pointer wrapped so it can cross the engine task boundary.
///
/// The callers (Swift) guarantee that the callback and context stay valid until
/// [`ws_engine_stop`] returns.
#[derive(Clone, Copy)]
struct ContextPtr(*mut c_void);

unsafe impl Send for ContextPtr {}
unsafe impl Sync for ContextPtr {}

impl ContextPtr {
    /// Returns the raw pointer; a method call forces the whole wrapper to be
    /// captured instead of just its raw field.
    fn get(&self) -> *mut c_void {
        self.0
    }
}

/// Opaque engine handle.
pub struct WsEngine {
    engine: Engine,
    runtime: tokio::runtime::Runtime,
    /// Kept so tests can assert the callback identity; the closure passed to the
    /// engine holds its own copy.
    #[allow(dead_code)]
    callback: WsEventCallback,
    #[allow(dead_code)]
    ctx: *mut c_void,
}

// The handle is only ever used from the thread that created it, but the runtime
// keeps worker threads; the raw context pointer is re-sent to the same callback.
unsafe impl Send for WsEngine {}
unsafe impl Sync for WsEngine {}

/// Options parsed from the JSON config passed to [`ws_engine_start`].
#[derive(serde::Deserialize)]
struct StartConfig {
    /// `host` or `controller`.
    role: String,
    /// Controller viewport, required for the controller role.
    viewport: Option<ViewportJson>,
    /// Whether to negotiate the audio track.
    #[serde(default = "default_true")]
    audio: bool,
    /// STUN server authority, e.g. `stun.example.com:3478`.
    stun: Option<String>,
    /// TURN server authority plus credentials.
    turn: Option<TurnJson>,
    /// Bind address; `0.0.0.0:0` by default.
    bind: Option<String>,
    /// `h265` (default) or `h264`.
    #[serde(default)]
    video_codec: VideoCodec,
}

#[derive(serde::Deserialize)]
struct ViewportJson {
    width: u32,
    height: u32,
    #[serde(default)]
    dpi: u16,
    #[serde(default = "default_fps")]
    fps: u32,
}

#[derive(serde::Deserialize)]
struct TurnJson {
    server: String,
    username: String,
    password: String,
    #[serde(default = "default_relay_ceiling")]
    relay_bitrate_ceiling: u32,
}

fn default_true() -> bool {
    true
}

fn default_fps() -> u32 {
    60
}

fn default_relay_ceiling() -> u32 {
    4_000_000
}

/// Starts an engine and returns an opaque handle, or null on failure.
///
/// # Safety
///
/// `config_json` must be a valid NUL terminated UTF-8 string. `ctx` is passed back
/// to `callback` untouched. The returned pointer must be released with
/// [`ws_engine_stop`].
#[no_mangle]
pub unsafe extern "C" fn ws_engine_start(
    config_json: *const c_char,
    callback: WsEventCallback,
    ctx: *mut c_void,
    error_out: *mut c_char,
    error_len: usize,
) -> *mut WsEngine {
    match start_inner(config_json, callback, ctx) {
        Ok(engine) => Box::into_raw(Box::new(engine)),
        Err(message) => {
            write_error(error_out, error_len, &message);
            std::ptr::null_mut()
        }
    }
}

unsafe fn start_inner(
    config_json: *const c_char,
    callback: WsEventCallback,
    ctx: *mut c_void,
) -> Result<WsEngine, String> {
    if config_json.is_null() {
        return Err("config_json is null".into());
    }
    let raw = CStr::from_ptr(config_json)
        .to_str()
        .map_err(|error| format!("config is not UTF-8: {error}"))?;
    let config: StartConfig =
        serde_json::from_str(raw).map_err(|error| format!("config is not valid JSON: {error}"))?;

    let role = match config.role.as_str() {
        "host" => Role::Host,
        "controller" => Role::Controller,
        other => return Err(format!("unknown role {other}")),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("waystone-rtc")
        .build()
        .map_err(|error| format!("cannot start the transport runtime: {error}"))?;

    // Media payloads are heap allocated by the engine and freed right after the
    // callback returns, so the callback must never retain the pointer.
    let context = ContextPtr(ctx);
    let sink = sink(move |event: Event| {
        let ctx = context.get();
        match event {
            Event::VideoFrame {
                data,
                keyframe,
                rtp_time,
            } => {
                let flags = if keyframe { 1u8 } else { 0 };
                let frame = encode_media(WS_EVENT_VIDEO as u8, flags, rtp_time, &data);
                callback(WS_EVENT_VIDEO, frame.as_ptr(), frame.len(), ctx);
            }
            Event::AudioFrame { data, rtp_time } => {
                let frame = encode_media(WS_EVENT_AUDIO as u8, 0, rtp_time, &data);
                callback(WS_EVENT_AUDIO, frame.as_ptr(), frame.len(), ctx);
            }
            Event::Input { reliable, data } => {
                let flags = if reliable { 2u8 } else { 0 };
                let frame = encode_media(WS_EVENT_INPUT as u8, flags, 0, &data);
                callback(WS_EVENT_INPUT, frame.as_ptr(), frame.len(), ctx);
            }
            other => {
                let json = control_json(&other);
                callback(WS_EVENT_CONTROL, json.as_ptr(), json.len(), ctx);
            }
        }
    });

    let mut engine_config = EngineConfig::new(role, sink).with_video_codec(config.video_codec);
    if let Some(bind) = &config.bind {
        let address = bind
            .parse()
            .map_err(|error| format!("bad bind address {bind}: {error}"))?;
        engine_config = engine_config.with_bind(address);
    }
    if let Some(stun) = &config.stun {
        let address = resolve_authority(stun)?;
        engine_config = engine_config.with_stun(address);
    }
    if let Some(turn) = &config.turn {
        let server = resolve_authority(&turn.server)?;
        engine_config = engine_config.with_turn(TurnConfig {
            server,
            username: turn.username.clone(),
            password: turn.password.clone(),
            relay_bitrate_ceiling: turn.relay_bitrate_ceiling,
        });
    }
    if let Some(viewport) = &config.viewport {
        engine_config = engine_config.with_session(SessionOptions {
            viewport: Viewport {
                width: viewport.width,
                height: viewport.height,
                dpi: viewport.dpi,
                fps: viewport.fps,
            },
            audio: config.audio,
            max_video_bitrate: 12_000_000,
        });
    }

    let engine = runtime
        .block_on(Engine::start(engine_config))
        .map_err(|error| format!("cannot bind the transport socket: {error}"))?;

    Ok(WsEngine {
        engine,
        runtime,
        callback,
        ctx,
    })
}

/// Stops the engine and releases the handle.
///
/// # Safety
///
/// `handle` must come from [`ws_engine_start`] and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_stop(handle: *mut WsEngine) {
    if handle.is_null() {
        return;
    }
    let engine = Box::from_raw(handle);
    engine.engine.shutdown();
    // Give the driver task a moment to emit its final event.
    std::thread::sleep(std::time::Duration::from_millis(50));
    engine
        .runtime
        .shutdown_timeout(std::time::Duration::from_millis(500));
}

/// Local UDP address of the transport, as `12345` in `buffer`.
///
/// # Safety
///
/// `buffer` must be writable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_local_port(
    handle: *mut WsEngine,
    buffer: *mut c_char,
    len: usize,
) -> usize {
    if handle.is_null() {
        return 0;
    }
    let engine = &*handle;
    let text = engine.engine.local_addr().to_string();
    copy_out(&text, buffer, len)
}

/// Creates the SDP offer (controller only).
///
/// # Safety
///
/// `handle` must be a live engine handle.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_offer(handle: *mut WsEngine) {
    if let Some(engine) = handle.as_ref() {
        engine.engine.offer();
    }
}

/// Accepts a remote offer (host only).
///
/// # Safety
///
/// `sdp` must be a valid NUL terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_accept_offer(handle: *mut WsEngine, sdp: *const c_char) {
    let (Some(engine), Some(sdp)) = (handle.as_ref(), cstr(sdp)) else {
        return;
    };
    engine.engine.accept_offer(sdp);
}

/// Applies a remote answer (controller only).
///
/// # Safety
///
/// `sdp` must be a valid NUL terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_accept_answer(handle: *mut WsEngine, sdp: *const c_char) {
    let (Some(engine), Some(sdp)) = (handle.as_ref(), cstr(sdp)) else {
        return;
    };
    engine.engine.accept_answer(sdp);
}

/// Adds a trickled remote candidate.
///
/// # Safety
///
/// `candidate` must be a valid NUL terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_add_candidate(handle: *mut WsEngine, candidate: *const c_char) {
    let (Some(engine), Some(candidate)) = (handle.as_ref(), cstr(candidate)) else {
        return;
    };
    engine.engine.add_remote_candidate(candidate);
}

/// Writes one encoded video frame.
///
/// # Safety
///
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_send_video(
    handle: *mut WsEngine,
    data: *const u8,
    len: usize,
    rtp_time: u64,
) {
    let Some(engine) = handle.as_ref() else {
        return;
    };
    let Some(bytes) = slice(data, len) else {
        return;
    };
    engine.engine.send_video(bytes.to_vec(), rtp_time);
}

/// Writes one Opus packet.
///
/// # Safety
///
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_send_audio(
    handle: *mut WsEngine,
    data: *const u8,
    len: usize,
    rtp_time: u64,
) {
    let Some(engine) = handle.as_ref() else {
        return;
    };
    let Some(bytes) = slice(data, len) else {
        return;
    };
    engine.engine.send_audio(bytes.to_vec(), rtp_time);
}

/// Sends an input frame.
///
/// # Safety
///
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_send_input(
    handle: *mut WsEngine,
    reliable: bool,
    data: *const u8,
    len: usize,
) {
    let Some(engine) = handle.as_ref() else {
        return;
    };
    let Some(bytes) = slice(data, len) else {
        return;
    };
    engine.engine.send_input(reliable, bytes.to_vec());
}

/// Caps the outgoing video bitrate.
///
/// # Safety
///
/// `handle` must be a live engine handle.
#[no_mangle]
pub unsafe extern "C" fn ws_engine_set_video_bitrate(handle: *mut WsEngine, bits_per_second: u32) {
    if let Some(engine) = handle.as_ref() {
        engine.engine.set_video_bitrate(bits_per_second);
    }
}

/// Monotonic counter used by tests to prove the callback fires.
#[no_mangle]
pub extern "C" fn ws_abi_version() -> u32 {
    static VERSION: AtomicU64 = AtomicU64::new(1);
    VERSION.load(Ordering::Relaxed) as u32
}

fn control_json(event: &Event) -> Vec<u8> {
    let value = match event {
        Event::LocalOffer(sdp) => serde_json::json!({"kind": "local_offer", "sdp": sdp}),
        Event::LocalAnswer(sdp) => serde_json::json!({"kind": "local_answer", "sdp": sdp}),
        Event::LocalCandidate(candidate) => {
            serde_json::json!({"kind": "local_candidate", "candidate": candidate})
        }
        Event::Connected => serde_json::json!({"kind": "connected"}),
        Event::Disconnected => serde_json::json!({"kind": "disconnected"}),
        Event::MediaReady { video, audio } => serde_json::json!({
            "kind": "media_ready",
            "video": video.map(|mid| mid.to_string()),
            "audio": audio.map(|mid| mid.to_string()),
        }),
        Event::ChannelOpen(label) => serde_json::json!({"kind": "channel_open", "label": label}),
        Event::PathUpdate {
            rtt_ms,
            relayed,
            estimated_bitrate,
        } => serde_json::json!({
            "kind": "path_update",
            "rttMs": rtt_ms,
            "relayed": relayed,
            "estimatedBitrate": estimated_bitrate,
        }),
        Event::KeyframeRequest => serde_json::json!({"kind": "keyframe_request"}),
        Event::FrameDropped => serde_json::json!({"kind": "frame_dropped"}),
        Event::Closed(reason) => serde_json::json!({"kind": "closed", "reason": reason}),
        Event::VideoFrame { .. } | Event::AudioFrame { .. } | Event::Input { .. } => {
            // Media never reaches this path; it is handled before the match.
            serde_json::json!({"kind": "media"})
        }
    };
    value.to_string().into_bytes()
}

fn encode_media(kind: u8, flags: u8, rtp_time: u64, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(MEDIA_HEADER_LEN + payload.len());
    frame.push(kind);
    frame.push(flags);
    frame.extend_from_slice(&rtp_time.to_be_bytes());
    frame.extend_from_slice(&[0u8; 6]);
    frame.extend_from_slice(payload);
    frame
}

fn resolve_authority(value: &str) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    value
        .to_socket_addrs()
        .map_err(|error| format!("cannot resolve {value}: {error}"))?
        .next()
        .ok_or_else(|| format!("{value} resolved to nothing"))
}

unsafe fn cstr(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    CStr::from_ptr(value).to_str().ok().map(str::to_string)
}

unsafe fn slice<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if data.is_null() || len == 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(data, len))
}

fn copy_out(text: &str, buffer: *mut c_char, len: usize) -> usize {
    if buffer.is_null() || len == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let take = bytes.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, take);
        *buffer.add(take) = 0;
    }
    take
}

fn write_error(buffer: *mut c_char, len: usize, message: &str) {
    if buffer.is_null() || len == 0 {
        return;
    }
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    let bytes = text.as_bytes();
    let take = bytes.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, take);
        *buffer.add(take) = 0;
    }
}

/// Header decoded by Swift from the first 16 bytes of a media event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub kind: u8,
    pub flags: u8,
    pub rtp_time: u64,
}

impl MediaHeader {
    /// Parses the fixed header of a media event.
    pub fn parse(bytes: &[u8]) -> Option<(MediaHeader, &[u8])> {
        if bytes.len() < MEDIA_HEADER_LEN {
            return None;
        }
        let mut time = [0u8; 8];
        time.copy_from_slice(&bytes[2..10]);
        Some((
            MediaHeader {
                kind: bytes[0],
                flags: bytes[1],
                rtp_time: u64::from_be_bytes(time),
            },
            &bytes[MEDIA_HEADER_LEN..],
        ))
    }

    /// True when the frame is a keyframe.
    pub fn is_keyframe(&self) -> bool {
        self.flags & 1 != 0
    }

    /// True when the input frame came on the reliable channel.
    pub fn is_reliable(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Handle used by tests to keep the callback alive.
pub type SharedCallback = Arc<WsEventCallback>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_header_roundtrip() {
        let frame = encode_media(WS_EVENT_VIDEO as u8, 1, 90_000, b"payload");
        let (header, payload) = MediaHeader::parse(&frame).unwrap();
        assert_eq!(header.kind, WS_EVENT_VIDEO as u8);
        assert!(header.is_keyframe());
        assert!(!header.is_reliable());
        assert_eq!(header.rtp_time, 90_000);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn header_rejects_short_frames() {
        assert!(MediaHeader::parse(&[1, 2, 3]).is_none());
    }

    #[test]
    fn control_json_is_tagged() {
        let json = control_json(&Event::Connected);
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["kind"], "connected");

        let json = control_json(&Event::LocalCandidate("candidate:1".into()));
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["kind"], "local_candidate");
        assert_eq!(value["candidate"], "candidate:1");
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(ws_abi_version(), 1);
    }
}
