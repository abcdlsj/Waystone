//! Windows: Session 0 privileged service + console-session worker (design doc
//! 4.1), DXGI desktop duplication, Media Foundation hardware encoding (4.3),
//! WASAPI loopback with Opus (4.4), `SendInput`, and deterministic rollback (4.5).

mod audio;
mod capture;
mod com;
mod desktop;
mod inject;
mod keymap;
mod security;
pub mod service;
mod session_spawn;
mod system;
pub mod tray;
mod video;
pub mod worker;

use std::path::{Path, PathBuf};

pub use capture::Capture;
pub use inject::post;
pub use system::{input_permitted, lock_screen, main_display, SpeakerMute};

pub const PLATFORM: waystone_proto::device::Platform = waystone_proto::device::Platform::Windows;

pub const INPUT_PERMISSION_HINT: &str = "not running as administrator or Windows service: \
     windows of elevated programs (some games, launchers and UAC) will ignore touches. \
     Install the privileged service with `waystone-host service install` from an elevated prompt.";

/// Per-monitor DPI awareness, so display rectangles, the cursor position and
/// injected coordinates are all physical pixels; and the terminal, if one
/// started this GUI-subsystem binary, for the CLI's output.
pub fn init() {
    system::become_dpi_aware();
    system::attach_parent_console();
}

/// `%ProgramData%\Waystone` so the Session 0 `LocalSystem` service and the
/// interactive user's tray/CLI share one machine-wide identity and configuration.
/// Falls back to `%LOCALAPPDATA%\Waystone` if `ProgramData` is unavailable.
pub fn state_dir() -> PathBuf {
    system::program_data()
        .or_else(system::local_app_data)
        .map(|base| base.join("Waystone"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Migrates any legacy `%LOCALAPPDATA%\Waystone` state into `target` if `target`
/// is not yet paired.
pub fn migrate_legacy_state(target: &Path) {
    system::migrate_legacy_state(target);
}

pub fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}
