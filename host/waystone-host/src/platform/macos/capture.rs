//! The Swift capture session of one stream.

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::oneshot;
use waystone_proto::VideoCodec;

use super::ffi;
use crate::capture::{FrameSink, MediaKind};
use crate::display::Display;
use crate::plan::StreamPlan;

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
    let kind = match kind {
        ffi::KIND_VIDEO => MediaKind::Video,
        ffi::KIND_AUDIO => MediaKind::Audio,
        _ => return,
    };
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
    /// Swift holds a raw pointer to it until `ws_capture_stop` returns.
    _sink: Arc<FrameSink>,
}

// The Swift session is internally synchronized; the handle is just a pointer.
unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

impl Capture {
    /// Starts capturing `display` and resolves once frames are flowing.
    pub async fn start(
        display: &Display,
        plan: &StreamPlan,
        codec: VideoCodec,
        audio: bool,
        sink: Arc<FrameSink>,
    ) -> Result<Capture> {
        let context = Arc::as_ptr(&sink) as *mut c_void;
        let handle = unsafe {
            ffi::ws_capture_create(
                display.id,
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
        let capture = Capture {
            handle,
            _sink: sink,
        };
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
