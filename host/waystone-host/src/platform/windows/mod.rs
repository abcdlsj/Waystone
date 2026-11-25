//! Windows: DXGI desktop duplication, Media Foundation hardware encoding,
//! WASAPI loopback with Opus, and `SendInput`.
//!
//! This is the in-session host (design doc 4.3, 4.4, 4.5): it runs in the
//! logged-in user's desktop. The Session 0 service that survives the lock
//! screen, UAC prompts and reboots (4.1) and the virtual display (4.2) are
//! separate work.

mod audio;
mod capture;
mod com;
mod inject;
mod keymap;
mod system;
pub mod tray;
mod video;

use std::path::PathBuf;

pub use capture::Capture;
pub use inject::post;
pub use system::{input_permitted, lock_screen, main_display, SpeakerMute};

pub const PLATFORM: waystone_proto::device::Platform = waystone_proto::device::Platform::Windows;

pub const INPUT_PERMISSION_HINT: &str = "not running as administrator: windows of programs \
     started as administrator (some games and launchers) will ignore touches. Run the host \
     from an elevated prompt to control them too.";

/// Per-monitor DPI awareness, so display rectangles, the cursor position and
/// injected coordinates are all physical pixels; and the terminal, if one
/// started this GUI-subsystem binary, for the CLI's output.
pub fn init() {
    system::become_dpi_aware();
    system::attach_parent_console();
}

/// `%LOCALAPPDATA%\Waystone`, from the shell rather than the environment:
/// a process started through WSL interop has no `LOCALAPPDATA`.
pub fn state_dir() -> PathBuf {
    system::local_app_data()
        .map(|base| base.join("Waystone"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}
