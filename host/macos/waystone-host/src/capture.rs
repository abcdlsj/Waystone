//! Bindings to the Swift capture layer, plus the frame plumbing into the engine.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use waystone_rtc::engine::Engine;

/// Callback signature shared with `swift/Capture.swift`.
pub type CaptureCallback = extern "C" fn(
    kind: i32,
    data: *const u8,
    len: usize,
    keyframe: bool,
    timestamp_micros: u64,
    context: *mut c_void,
);

extern "C" {
    /// Starts capture; returns 0 on success.
    fn ws_capture_start(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        callback: CaptureCallback,
        context: *mut c_void,
    ) -> i32;
    fn ws_capture_stop();
    fn ws_capture_request_keyframe();
    fn ws_capture_set_bitrate(bitrate: i32);
}

/// Event kind for video frames, matching `Capture.swift`.
const KIND_VIDEO: i32 = 0;
/// Event kind for audio packets.
const KIND_AUDIO: i32 = 1;

/// Shared state between the capture callback and the agent.
#[derive(Default)]
pub struct FrameBridge {
    engine: Mutex<Option<Engine>>,
    /// Video timestamp origin, so RTP time starts at zero.
    video_base: AtomicU64,
    audio_base: AtomicU64,
    video_seen: AtomicBool,
    audio_seen: AtomicBool,
    /// Frames produced by the encoder, whether or not an engine was attached.
    video_frames: AtomicU64,
    /// Audio packets produced by the encoder.
    audio_frames: AtomicU64,
}

impl FrameBridge {
    pub fn new() -> Arc<FrameBridge> {
        Arc::new(FrameBridge::default())
    }

    /// Installs the engine once the session is up.
    pub fn attach(&self, engine: Engine) {
        *self.engine.lock().unwrap() = Some(engine);
    }

    /// Drops the engine when the session ends.
    pub fn detach(&self) {
        *self.engine.lock().unwrap() = None;
    }

    fn forward_video(&self, data: &[u8], timestamp_micros: u64) {
        let Some(engine) = self.engine.lock().unwrap().clone() else {
            let missing = self.video_frames.fetch_add(1, Ordering::Relaxed);
            if missing < 3 {
                tracing::debug!("capture frame with no engine attached yet");
            }
            return;
        };
        let rtp_time = self.rtp_time(timestamp_micros, &self.video_seen, &self.video_base, 90);
        engine.send_video(data.to_vec(), rtp_time);
    }

    fn forward_audio(&self, data: &[u8], timestamp_micros: u64) {
        let Some(engine) = self.engine.lock().unwrap().clone() else {
            return;
        };
        let rtp_time = self.rtp_time(timestamp_micros, &self.audio_seen, &self.audio_base, 48);
        engine.send_audio(data.to_vec(), rtp_time);
    }

    /// Number of video frames forwarded so far.
    pub fn video_frame_count(&self) -> u64 {
        self.video_frames.load(Ordering::Relaxed)
    }

    /// Number of audio packets forwarded so far.
    pub fn audio_frame_count(&self) -> u64 {
        self.audio_frames.load(Ordering::Relaxed)
    }

    /// Converts a capture timestamp into RTP time, rebasing on the first frame so
    /// the receiver always starts at zero.
    fn rtp_time(
        &self,
        timestamp_micros: u64,
        seen: &AtomicBool,
        base: &AtomicU64,
        clock_khz: u64,
    ) -> u64 {
        if !seen.swap(true, Ordering::Relaxed) {
            base.store(timestamp_micros, Ordering::Relaxed);
        }
        let origin = base.load(Ordering::Relaxed);
        let delta = timestamp_micros.saturating_sub(origin);
        // 90 kHz for video, 48 kHz for Opus.
        delta * clock_khz / 1000
    }
}

/// Trampoline installed into the Swift layer.
extern "C" fn on_capture_event(
    kind: i32,
    data: *const u8,
    len: usize,
    _keyframe: bool,
    timestamp_micros: u64,
    context: *mut c_void,
) {
    if data.is_null() || len == 0 || context.is_null() {
        return;
    }
    // The bridge outlives the capture session, so the pointer stays valid.
    let bridge = unsafe { &*(context as *const FrameBridge) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    // Counters are bumped before forwarding so `capture-test` can tell "the
    // encoder produced nothing" from "no engine was attached yet".
    match kind {
        KIND_VIDEO => {
            bridge.video_frames.fetch_add(1, Ordering::Relaxed);
            bridge.forward_video(bytes, timestamp_micros);
        }
        KIND_AUDIO => {
            bridge.audio_frames.fetch_add(1, Ordering::Relaxed);
            bridge.forward_audio(bytes, timestamp_micros);
        }
        _ => {}
    }
}

/// Manages the lifetime of the Swift capture session.
pub struct Capture {
    bridge: Arc<FrameBridge>,
    running: AtomicBool,
}

impl Capture {
    /// Prepares a capture session bound to `bridge`.
    pub fn new(bridge: Arc<FrameBridge>) -> Capture {
        Capture {
            bridge,
            running: AtomicBool::new(false),
        }
    }

    /// Starts capturing with the given geometry.
    ///
    /// Blocks until the first frame can be produced, because ScreenCaptureKit
    /// answers asynchronously and the session must not answer the offer before
    /// the encoder exists.
    pub fn start(&self, width: u32, height: u32, fps: u32, bitrate: u32) -> anyhow::Result<()> {
        let context = Arc::as_ptr(&self.bridge) as *mut c_void;
        let status = unsafe {
            ws_capture_start(
                width as i32,
                height as i32,
                fps as i32,
                bitrate as i32,
                on_capture_event,
                context,
            )
        };
        if status != 0 {
            anyhow::bail!("capture failed to start (status {status})");
        }
        self.running.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stops capturing.
    pub fn stop(&self) {
        if self.running.swap(false, Ordering::Relaxed) {
            unsafe { ws_capture_stop() };
        }
    }

    /// Asks the encoder for an IDR frame.
    pub fn request_keyframe(&self) {
        if self.running.load(Ordering::Relaxed) {
            unsafe { ws_capture_request_keyframe() };
        }
    }

    /// Updates the encoder bitrate.
    pub fn set_bitrate(&self, bits_per_second: u32) {
        if self.running.load(Ordering::Relaxed) {
            unsafe { ws_capture_set_bitrate(bits_per_second.min(i32::MAX as u32) as i32) };
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let bridge = FrameBridge::new();
        assert_eq!(bridge.video_frame_count(), 0);
        assert_eq!(bridge.audio_frame_count(), 0);
    }

    #[test]
    fn frames_are_dropped_without_an_engine() {
        let bridge = FrameBridge::new();
        // Nothing is attached yet, so the calls must be no-ops, not panics.
        bridge.forward_video(b"frame", 1_000_000);
        bridge.forward_audio(b"packet", 1_000_000);
        assert!(!bridge.video_seen.load(Ordering::Relaxed));
        assert!(!bridge.audio_seen.load(Ordering::Relaxed));
    }

    #[test]
    fn rtp_time_is_rebased_on_the_first_frame() {
        let bridge = FrameBridge::new();
        let seen = AtomicBool::new(false);
        let base = AtomicU64::new(0);

        // A 33 ms delta at 90 kHz is 2970 ticks.
        assert_eq!(bridge.rtp_time(5_000_000, &seen, &base, 90), 0);
        assert_eq!(bridge.rtp_time(5_033_000, &seen, &base, 90), 2_970);

        // A 20 ms delta at 48 kHz is 960 ticks, exactly one Opus frame.
        let seen = AtomicBool::new(false);
        let base = AtomicU64::new(0);
        assert_eq!(bridge.rtp_time(1_000_000, &seen, &base, 48), 0);
        assert_eq!(bridge.rtp_time(1_020_000, &seen, &base, 48), 960);
    }

    #[test]
    fn capture_lifecycle_is_idempotent() {
        let bridge = FrameBridge::new();
        let capture = Capture::new(bridge);
        // Stopping a session that never started must not call into Swift.
        capture.stop();
        capture.request_keyframe();
        capture.set_bitrate(1_000_000);
    }
}

/// Decodes and injects one input frame into the local desktop.
///
/// Returns false when the frame is malformed or the platform rejected it.
pub fn inject_input(frame: &[u8]) -> bool {
    if frame.is_empty() {
        return false;
    }
    extern "C" {
        fn ws_input_handle(data: *const u8, length: usize) -> i32;
    }
    unsafe { ws_input_handle(frame.as_ptr(), frame.len()) == 0 }
}

#[cfg(test)]
mod input_tests {
    use super::inject_input;
    use waystone_proto::input::InputEvent;

    #[test]
    fn malformed_frames_are_rejected_before_touching_the_os() {
        assert!(!inject_input(&[]));
        // Unknown opcode.
        assert!(!inject_input(&[0x7f]));
        // Truncated mouse move.
        assert!(!inject_input(&[0x01, 0x00]));
    }

    #[test]
    fn a_move_frame_decodes() {
        let frame = InputEvent::MouseMove { x: 32768, y: 16384 }.to_bytes();
        // Injection needs a real desktop session, so only the length and opcode
        // are asserted here; the Swift side is covered by its own compile step.
        assert_eq!(frame.len(), 5);
        assert_eq!(frame[0], 0x01);
    }
}
