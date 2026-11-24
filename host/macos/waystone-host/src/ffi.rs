//! Declarations of the Swift media layer (`swift/*.swift`, `@_cdecl`).
//!
//! Threading contract: every function may be called from any thread. Frame
//! callbacks run on private capture queues, and never after
//! `ws_capture_stop` has returned. `ws_capture_start` reports through its
//! completion callback exactly once.

use std::ffi::c_void;

/// `kind` is [`KIND_VIDEO`] (an Annex-B access unit) or [`KIND_AUDIO`] (one
/// 20 ms Opus packet); `micros` is the presentation time on the host clock.
pub type FrameCallback = extern "C" fn(
    kind: i32,
    data: *const u8,
    len: usize,
    keyframe: bool,
    micros: u64,
    context: *mut c_void,
);

/// `status` is 0 once capturing, or one of the `CAPTURE_*` errors.
pub type DoneCallback = extern "C" fn(status: i32, context: *mut c_void);

pub const KIND_VIDEO: i32 = 0;
pub const KIND_AUDIO: i32 = 1;

pub const CAPTURE_NO_DISPLAY: i32 = -1;
pub const CAPTURE_DISPLAY_ASLEEP: i32 = -2;
pub const CAPTURE_NO_ENCODER: i32 = -3;

#[allow(clippy::too_many_arguments)]
extern "C" {
    pub fn ws_capture_create(
        display: u32,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        codec: i32,
        audio: bool,
        on_frame: FrameCallback,
        context: *mut c_void,
    ) -> *mut c_void;
    pub fn ws_capture_start(capture: *mut c_void, done: DoneCallback, context: *mut c_void);
    /// Stops and releases; the handle is dangling afterwards.
    pub fn ws_capture_stop(capture: *mut c_void);
    pub fn ws_capture_request_keyframe(capture: *mut c_void);
    pub fn ws_capture_set_bitrate(capture: *mut c_void, bitrate: i32);
    pub fn ws_capture_set_fps(capture: *mut c_void, fps: i32);
    pub fn ws_capture_set_audio(capture: *mut c_void, enabled: bool);

    pub fn ws_display_main() -> u32;
    pub fn ws_display_info(
        display: u32,
        x: *mut f64,
        y: *mut f64,
        width: *mut f64,
        height: *mut f64,
        pixel_width: *mut i32,
        pixel_height: *mut i32,
    ) -> bool;

    /// `held_button` is -1 for none, else the button whose drag this is.
    pub fn ws_inject_mouse_move(x: f64, y: f64, held_button: i32, modifiers: u8);
    pub fn ws_inject_mouse_button(
        button: i32,
        down: bool,
        x: f64,
        y: f64,
        clicks: i32,
        modifiers: u8,
    );
    pub fn ws_inject_scroll(dx: i32, dy: i32);
    pub fn ws_inject_key(virtual_key: u16, down: bool, modifiers: u8);
    pub fn ws_inject_text(utf8: *const u8, len: usize);

    pub fn ws_accessibility_trusted(prompt: bool) -> bool;
    pub fn ws_lock_screen() -> bool;
    pub fn ws_output_muted() -> i32;
    pub fn ws_set_output_muted(muted: bool) -> bool;
}
