//! Which display is streamed, and what a session changes and puts back
//! (design doc 4.4 and 4.5).

use tracing::{info, warn};

use super::ffi;
use crate::display::Display;

pub fn main_display() -> Option<Display> {
    display_by_id(unsafe { ffi::ws_display_main() })
}

fn display_by_id(id: u32) -> Option<Display> {
    let (mut x, mut y, mut width, mut height) = (0.0, 0.0, 0.0, 0.0);
    let (mut pixel_width, mut pixel_height) = (0, 0);
    let found = unsafe {
        ffi::ws_display_info(
            id,
            &mut x,
            &mut y,
            &mut width,
            &mut height,
            &mut pixel_width,
            &mut pixel_height,
        )
    };
    found.then(|| Display {
        id,
        origin: (x, y),
        size: (width, height),
        pixels: (pixel_width.max(1) as u32, pixel_height.max(1) as u32),
    })
}

/// True when this process may post input events; asks the user once.
pub fn input_permitted(prompt: bool) -> bool {
    unsafe { ffi::ws_accessibility_trusted(prompt) }
}

pub fn lock_screen() {
    info!("locking the screen");
    if !unsafe { ffi::ws_lock_screen() } {
        warn!("could not lock the screen");
    }
}

/// Mutes the speakers for the length of a session and restores them after.
///
/// The original state is read first, so a machine that was already muted
/// stays muted, and one whose device has no mute control is left alone.
pub struct SpeakerMute {
    restore: Option<bool>,
}

impl SpeakerMute {
    pub fn engage() -> SpeakerMute {
        let restore = match unsafe { ffi::ws_output_muted() } {
            0 => {
                if unsafe { ffi::ws_set_output_muted(true) } {
                    info!("speakers muted for the session");
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        };
        SpeakerMute { restore }
    }
}

impl Drop for SpeakerMute {
    fn drop(&mut self) {
        if let Some(muted) = self.restore.take() {
            unsafe { ffi::ws_set_output_muted(muted) };
            info!("speakers restored");
        }
    }
}
