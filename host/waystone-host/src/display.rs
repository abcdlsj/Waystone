//! The display being streamed.

/// Where the display sits in the coordinate space injected pointer events
/// use, and how many pixels its panel really has.
///
/// On macOS the space is global display points; on Windows it is virtual
/// desktop pixels (the process is per-monitor DPI aware). Either way a
/// normalized `0..=65535` position maps linearly onto it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    /// Platform handle: a `CGDirectDisplayID`, or a DXGI output index.
    pub id: u32,
    pub origin: (f64, f64),
    pub size: (f64, f64),
    pub pixels: (u32, u32),
}

impl Display {
    /// Maps a normalized `0..=65535` position onto this display.
    pub fn point(&self, x: u16, y: u16) -> (f64, f64) {
        let max = f64::from(u16::MAX);
        (
            self.origin.0 + self.size.0 * f64::from(x) / max,
            self.origin.1 + self.size.1 * f64::from(y) / max,
        )
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
