//! macOS: ScreenCaptureKit, VideoToolbox and CoreGraphics through the Swift
//! layer in `swift/` (compiled by `build.rs`, declared in `ffi`).

mod capture;
mod ffi;
mod inject;
mod keymap;
mod system;

use std::path::PathBuf;

pub use capture::Capture;
pub use inject::post;
pub use system::{input_permitted, lock_screen, main_display, SpeakerMute};

pub const PLATFORM: waystone_proto::device::Platform = waystone_proto::device::Platform::Macos;

pub const INPUT_PERMISSION_HINT: &str = "Accessibility permission is missing: the stream will \
     work but touches will not. Allow this binary in System Settings > Privacy & Security > \
     Accessibility.";

pub fn init() {}

/// `~/Library/Application Support/Waystone`.
pub fn state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/Waystone"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    let status = unsafe { gethostname(buffer.as_mut_ptr() as *mut i8, buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
    Some(String::from_utf8_lossy(&buffer[..end]).to_string())
}

// `gethostname` is not exposed by std, and pulling in the whole libc crate for
// one call is not worth it.
extern "C" {
    fn gethostname(name: *mut i8, len: usize) -> i32;
}
