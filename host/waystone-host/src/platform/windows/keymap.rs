//! USB HID usages (what the protocol carries) to PC set 1 scan codes.
//!
//! Scan codes rather than virtual keys: games read raw input and DirectInput,
//! which see scan codes, and a scan code means the same physical key whatever
//! the keyboard layout.

/// A scan code and whether it needs the `E0` (extended) prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub code: u16,
    pub extended: bool,
}

const fn plain(code: u16) -> ScanCode {
    ScanCode {
        code,
        extended: false,
    }
}

const fn extended(code: u16) -> ScanCode {
    ScanCode {
        code,
        extended: true,
    }
}

pub fn scan_code(usage: u32) -> Option<ScanCode> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, // a..m
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // n..z
    ];
    // Keypad 1..9, then 0 and the decimal point.
    const KEYPAD: [u16; 11] = [
        0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49, 0x52, 0x53,
    ];
    Some(match usage {
        0x04..=0x1d => plain(LETTERS[(usage - 0x04) as usize]),
        // 1..9 are 0x02..0x0A and 0 is 0x0B, exactly HID order.
        0x1e..=0x27 => plain(0x02 + (usage - 0x1e) as u16),
        0x28 => plain(0x1C),                                // Enter
        0x29 => plain(0x01),                                // Escape
        0x2a => plain(0x0E),                                // Backspace
        0x2b => plain(0x0F),                                // Tab
        0x2c => plain(0x39),                                // Space
        0x2d => plain(0x0C),                                // -
        0x2e => plain(0x0D),                                // =
        0x2f => plain(0x1A),                                // [
        0x30 => plain(0x1B),                                // ]
        0x31 => plain(0x2B),                                // backslash
        0x33 => plain(0x27),                                // ;
        0x34 => plain(0x28),                                // '
        0x35 => plain(0x29),                                // `
        0x36 => plain(0x33),                                // ,
        0x37 => plain(0x34),                                // .
        0x38 => plain(0x35),                                // /
        0x39 => plain(0x3A),                                // Caps Lock
        0x3a..=0x43 => plain(0x3B + (usage - 0x3a) as u16), // F1..F10
        0x44 => plain(0x57),                                // F11
        0x45 => plain(0x58),                                // F12
        0x46 => extended(0x37),                             // Print Screen
        0x47 => plain(0x46),                                // Scroll Lock
        0x49 => extended(0x52),                             // Insert
        0x4a => extended(0x47),                             // Home
        0x4b => extended(0x49),                             // Page Up
        0x4c => extended(0x53),                             // Delete
        0x4d => extended(0x4F),                             // End
        0x4e => extended(0x51),                             // Page Down
        0x4f => extended(0x4D),                             // Right
        0x50 => extended(0x4B),                             // Left
        0x51 => extended(0x50),                             // Down
        0x52 => extended(0x48),                             // Up
        0x53 => plain(0x45),                                // Num Lock
        0x54 => extended(0x35),                             // keypad /
        0x55 => plain(0x37),                                // keypad *
        0x56 => plain(0x4A),                                // keypad -
        0x57 => plain(0x4E),                                // keypad +
        0x58 => extended(0x1C),                             // keypad Enter
        0x59..=0x63 => plain(KEYPAD[(usage - 0x59) as usize]),
        0x64 => plain(0x56),    // the extra key left of Z on ISO keyboards
        0x65 => extended(0x5D), // Application (context menu)
        0xe0 => plain(0x1D),    // Left Control
        0xe1 => plain(0x2A),    // Left Shift
        0xe2 => plain(0x38),    // Left Alt
        0xe3 => extended(0x5B), // Left Windows
        0xe4 => extended(0x1D), // Right Control
        0xe5 => plain(0x36),    // Right Shift
        0xe6 => extended(0x38), // Right Alt
        0xe7 => extended(0x5C), // Right Windows
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use waystone_proto::input::keys;

    #[test]
    fn maps_the_keys_presets_use() {
        assert_eq!(scan_code(keys::letter('w').unwrap()), Some(plain(0x11)));
        assert_eq!(scan_code(keys::letter('a').unwrap()), Some(plain(0x1E)));
        assert_eq!(scan_code(keys::letter('s').unwrap()), Some(plain(0x1F)));
        assert_eq!(scan_code(keys::letter('d').unwrap()), Some(plain(0x20)));
        assert_eq!(scan_code(keys::letter('f').unwrap()), Some(plain(0x21)));
        assert_eq!(scan_code(keys::letter('m').unwrap()), Some(plain(0x32)));
        assert_eq!(scan_code(keys::Z), Some(plain(0x2C)));
        assert_eq!(scan_code(keys::DIGIT_1), Some(plain(0x02)));
        assert_eq!(scan_code(keys::DIGIT_0), Some(plain(0x0B)));
        assert_eq!(scan_code(keys::SPACE), Some(plain(0x39)));
        assert_eq!(scan_code(keys::ENTER), Some(plain(0x1C)));
        assert_eq!(scan_code(keys::ESCAPE), Some(plain(0x01)));
        assert_eq!(scan_code(keys::F1), Some(plain(0x3B)));
        assert_eq!(scan_code(keys::F12), Some(plain(0x58)));
        assert_eq!(scan_code(keys::LEFT_CONTROL), Some(plain(0x1D)));
        assert_eq!(scan_code(keys::UP), Some(extended(0x48)));
        assert_eq!(scan_code(0xff), None);
    }
}
