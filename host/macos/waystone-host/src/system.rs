//! The system side of a session: which display is streamed, and what has to
//! be put back afterwards (design doc 4.4 and 4.5).

use tracing::{info, warn};

use crate::ffi;

/// The display being streamed: where it sits in global coordinates (what
/// injected events use) and how many pixels the panel really has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub id: u32,
    pub origin: (f64, f64),
    pub size: (f64, f64),
    pub pixels: (u32, u32),
}

impl Display {
    pub fn main() -> Option<Display> {
        Display::by_id(unsafe { ffi::ws_display_main() })
    }

    pub fn by_id(id: u32) -> Option<Display> {
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

    /// Maps a normalized `0..=65535` position onto this display.
    pub fn point(&self, x: u16, y: u16) -> (f64, f64) {
        let max = f64::from(u16::MAX);
        (
            self.origin.0 + self.size.0 * f64::from(x) / max,
            self.origin.1 + self.size.1 * f64::from(y) / max,
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_normalized_positions_onto_the_display() {
        let display = Display {
            id: 1,
            origin: (-1512.0, 0.0),
            size: (1512.0, 982.0),
            pixels: (3024, 1964),
        };
        assert_eq!(display.point(0, 0), (-1512.0, 0.0));
        assert_eq!(display.point(u16::MAX, u16::MAX), (0.0, 982.0));
        let (x, _) = display.point(32768, 0);
        assert!((x - -755.98).abs() < 0.1);
    }
}
