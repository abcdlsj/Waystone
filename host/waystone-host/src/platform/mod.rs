//! Everything that differs between host operating systems.
//!
//! Each platform module provides the same items, and the rest of the agent
//! uses only these:
//!
//! * `PLATFORM`, `state_dir()`, `hostname()` – identity and storage;
//! * `main_display()` – the display a session streams;
//! * `Capture` – `start(display, plan, codec, audio, sink)` resolves once
//!   frames flow into the [`FrameSink`](crate::capture::FrameSink); then
//!   `request_keyframe`, `set_bitrate`, `set_frame_rate`, `set_audio`;
//!   dropping it stops capture, and no frame is delivered afterwards;
//! * `post(&Action)` – injects one decided input action (called on the
//!   injector thread only);
//! * `input_permitted(prompt)` and `INPUT_PERMISSION_HINT`;
//! * `lock_screen()` and `SpeakerMute::engage()`, which restores on drop;
//! * `init()` – process-wide setup, run once before anything else.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

// The key tables are pure, so their tests run on any development machine.
#[cfg(all(test, not(windows)))]
#[path = "windows/keymap.rs"]
mod windows_keymap;
