//! What to encode for one session: picture size and bitrate.

use waystone_proto::Viewport;

use crate::display::Display;

/// Encoded size and target bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPlan {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// Longest edge any stream may have, whatever the controller asks for.
const MAX_EDGE: u32 = 3840;
/// Below this the picture turns to mush whatever the size.
const MIN_BITRATE: u32 = 1_500_000;
const REFERENCE_PIXELS: u64 = 1920 * 1080;

/// Fits the display into the controller's screen without changing its shape.
///
/// The encoded picture always has the display's aspect ratio, so the full
/// `0..=65535` input range maps onto the full display and the phone can
/// letterbox it exactly; padding inside the frame would shift every click.
/// Within that shape the stream is as large as the phone draws it (encoding
/// more pixels than the phone shows is waste) but never larger than the
/// panel's own pixels (that would invent detail).
///
/// `budget` is the bitrate for 1080p at 60 fps; other sizes and rates scale
/// linearly from it.
pub fn plan(display: &Display, viewport: Option<&Viewport>, budget: u32) -> StreamPlan {
    let (panel_width, panel_height) = display.pixels;
    let fps = viewport.map(Viewport::frame_rate_tier).unwrap_or(60);
    let (box_width, box_height) = match viewport {
        Some(viewport) if viewport.width > 0 && viewport.height > 0 => {
            (viewport.width, viewport.height)
        }
        _ => (panel_width, panel_height),
    };

    let fit = (box_width as f64 / panel_width as f64)
        .min(box_height as f64 / panel_height as f64)
        .min(1.0)
        .min(MAX_EDGE as f64 / panel_width.max(panel_height) as f64);
    // Encoders work on even dimensions.
    let width = ((panel_width as f64 * fit).round() as u32).max(2) & !1;
    let height = ((panel_height as f64 * fit).round() as u32).max(2) & !1;

    let pixels = u64::from(width) * u64::from(height);
    let scaled = u64::from(budget) * pixels * u64::from(fps) / (REFERENCE_PIXELS * 60);
    let bitrate = (scaled as u32).clamp(MIN_BITRATE.min(budget), budget.saturating_mul(2));

    StreamPlan {
        width,
        height,
        fps,
        bitrate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina_macbook() -> Display {
        Display {
            id: 1,
            origin: (0.0, 0.0),
            size: (1512.0, 982.0),
            pixels: (3024, 1964),
        }
    }

    fn viewport(width: u32, height: u32, fps: u32) -> Viewport {
        Viewport {
            width,
            height,
            dpi: 460,
            fps,
        }
    }

    #[test]
    fn keeps_the_display_shape_inside_a_wide_phone() {
        // iPhone 16 Pro landscape, 19.5:9; the 16:10.4 desktop is pillarboxed
        // on the phone, never stretched or padded in the frame.
        let planned = plan(
            &retina_macbook(),
            Some(&viewport(2622, 1206, 60)),
            8_000_000,
        );
        assert_eq!(planned.height, 1206);
        assert_eq!(planned.width, 1856);
        let aspect = planned.width as f64 / planned.height as f64;
        assert!((aspect - 3024.0 / 1964.0).abs() < 0.01);
    }

    #[test]
    fn never_exceeds_the_panel_or_the_edge_cap() {
        let display = Display {
            pixels: (1920, 1080),
            ..retina_macbook()
        };
        let planned = plan(&display, Some(&viewport(2732, 2048, 60)), 8_000_000);
        assert_eq!((planned.width, planned.height), (1920, 1080));
        assert_eq!(planned.bitrate, 8_000_000);

        let huge = Display {
            pixels: (6016, 3384),
            ..retina_macbook()
        };
        let planned = plan(&huge, None, 8_000_000);
        assert_eq!(planned.width, 3840);
    }

    #[test]
    fn bitrate_scales_with_pixels_and_frame_rate_within_bounds() {
        let display = Display {
            pixels: (1920, 1080),
            ..retina_macbook()
        };
        let smooth = plan(&display, Some(&viewport(1920, 1080, 60)), 8_000_000);
        let cool = plan(&display, Some(&viewport(1920, 1080, 45)), 8_000_000);
        assert_eq!(cool.fps, 45);
        assert!(cool.bitrate < smooth.bitrate);

        let tiny = plan(&display, Some(&viewport(320, 180, 60)), 8_000_000);
        assert_eq!(tiny.bitrate, MIN_BITRATE);
        assert_eq!(tiny.width % 2, 0);
    }
}
