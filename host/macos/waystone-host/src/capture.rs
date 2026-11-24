//! The Swift capture session of one stream, and the frame path into the engine.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use tokio::sync::oneshot;
use waystone_proto::VideoCodec;
use waystone_rtc::Engine;

use crate::ffi;
use crate::plan::StreamPlan;

/// Where encoded frames go. Lives as long as the capture session that calls
/// into it, and converts host clock timestamps into RTP time.
#[derive(Default)]
pub struct FrameSink {
    engine: Mutex<Option<Engine>>,
    video_origin: Mutex<Option<u64>>,
    audio_origin: Mutex<Option<u64>>,
    video_frames: AtomicU64,
    audio_packets: AtomicU64,
}

impl FrameSink {
    pub fn new(engine: Option<Engine>) -> Arc<FrameSink> {
        Arc::new(FrameSink {
            engine: Mutex::new(engine),
            ..FrameSink::default()
        })
    }

    pub fn counts(&self) -> (u64, u64) {
        (
            self.video_frames.load(Ordering::Relaxed),
            self.audio_packets.load(Ordering::Relaxed),
        )
    }

    fn deliver(&self, kind: i32, data: &[u8], micros: u64) {
        let (origin, counter, clock_khz) = match kind {
            ffi::KIND_VIDEO => (&self.video_origin, &self.video_frames, 90),
            ffi::KIND_AUDIO => (&self.audio_origin, &self.audio_packets, 48),
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let rtp_time = rtp_time(origin, micros, clock_khz);
        let Some(engine) = self.engine.lock().unwrap().clone() else {
            return;
        };
        if kind == ffi::KIND_VIDEO {
            engine.send_video(data.to_vec(), rtp_time);
        } else {
            engine.send_audio(data.to_vec(), rtp_time);
        }
    }
}

/// RTP time relative to the first timestamp seen: 90 kHz video, 48 kHz Opus.
fn rtp_time(origin: &Mutex<Option<u64>>, micros: u64, clock_khz: u64) -> u64 {
    let base = *origin.lock().unwrap().get_or_insert(micros);
    micros.saturating_sub(base) * clock_khz / 1000
}

extern "C" fn on_frame(
    kind: i32,
    data: *const u8,
    len: usize,
    _keyframe: bool,
    micros: u64,
    context: *mut c_void,
) {
    if data.is_null() || len == 0 || context.is_null() {
        return;
    }
    // The capture session holds a strong reference (see `Capture::start`)
    // until `ws_capture_stop` returns, after which no callback runs.
    let sink = unsafe { &*(context as *const FrameSink) };
    sink.deliver(
        kind,
        unsafe { std::slice::from_raw_parts(data, len) },
        micros,
    );
}

extern "C" fn on_started(status: i32, context: *mut c_void) {
    let sender = unsafe { Box::from_raw(context as *mut oneshot::Sender<i32>) };
    let _ = sender.send(status);
}

/// A running capture session; stopping it is dropping it.
pub struct Capture {
    handle: *mut c_void,
    sink: Arc<FrameSink>,
}

// The Swift session is internally synchronized; the handle is just a pointer.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    /// Starts capturing `display` and resolves once frames are flowing.
    pub async fn start(
        display: u32,
        plan: &StreamPlan,
        codec: VideoCodec,
        audio: bool,
        sink: Arc<FrameSink>,
    ) -> Result<Capture> {
        let context = Arc::as_ptr(&sink) as *mut c_void;
        let handle = unsafe {
            ffi::ws_capture_create(
                display,
                plan.width as i32,
                plan.height as i32,
                plan.fps as i32,
                plan.bitrate.min(i32::MAX as u32) as i32,
                match codec {
                    VideoCodec::H265 => 0,
                    VideoCodec::H264 => 1,
                },
                audio,
                on_frame,
                context,
            )
        };
        // From here `Drop` stops the session, including on the error paths.
        let capture = Capture { handle, sink };
        let (sender, receiver) = oneshot::channel::<i32>();
        let done = Box::into_raw(Box::new(sender)) as *mut c_void;
        unsafe { ffi::ws_capture_start(handle, on_started, done) };
        match receiver.await.unwrap_or(-4) {
            0 => Ok(capture),
            ffi::CAPTURE_NO_DISPLAY => bail!("no display to capture"),
            ffi::CAPTURE_DISPLAY_ASLEEP => bail!("the display is asleep"),
            ffi::CAPTURE_NO_ENCODER => bail!("no hardware video encoder is available"),
            _ => bail!(
                "screen capture failed; grant Screen Recording to this binary in System Settings"
            ),
        }
    }

    pub fn counts(&self) -> (u64, u64) {
        self.sink.counts()
    }

    pub fn request_keyframe(&self) {
        unsafe { ffi::ws_capture_request_keyframe(self.handle) }
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        unsafe { ffi::ws_capture_set_bitrate(self.handle, bitrate.min(i32::MAX as u32) as i32) }
    }

    pub fn set_frame_rate(&self, fps: u32) {
        unsafe { ffi::ws_capture_set_fps(self.handle, fps as i32) }
    }

    pub fn set_audio(&self, enabled: bool) {
        unsafe { ffi::ws_capture_set_audio(self.handle, enabled) }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Blocks for at most a few seconds while ScreenCaptureKit winds down;
        // afterwards the sink is no longer referenced from Swift.
        unsafe { ffi::ws_capture_stop(self.handle) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_time_starts_at_zero_and_follows_the_clock() {
        let origin = Mutex::new(None);
        assert_eq!(rtp_time(&origin, 5_000_000, 90), 0);
        // 33 ms at 90 kHz.
        assert_eq!(rtp_time(&origin, 5_033_000, 90), 2_970);

        let origin = Mutex::new(None);
        assert_eq!(rtp_time(&origin, 1_000_000, 48), 0);
        // One 20 ms Opus packet at 48 kHz.
        assert_eq!(rtp_time(&origin, 1_020_000, 48), 960);
    }

    #[test]
    fn frames_without_an_engine_are_counted_and_dropped() {
        let sink = FrameSink::new(None);
        sink.deliver(ffi::KIND_VIDEO, b"frame", 1);
        sink.deliver(ffi::KIND_AUDIO, b"packet", 1);
        assert_eq!(sink.counts(), (1, 1));
    }
}
