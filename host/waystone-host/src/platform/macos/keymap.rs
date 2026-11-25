//! USB HID usages (what the protocol carries) to macOS virtual key codes.

/// The macOS virtual key code for a HID usage on the Keyboard page.
pub fn virtual_key(usage: u32) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, // a..m
        0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06, // n..z
    ];
    // 1..9 then 0, in HID order.
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    const FUNCTIONS: [u16; 12] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
    ];
    Some(match usage {
        0x04..=0x1d => LETTERS[(usage - 0x04) as usize],
        0x1e..=0x27 => DIGITS[(usage - 0x1e) as usize],
        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape
        0x2a => 0x33, // Delete (backspace)
        0x2b => 0x30, // Tab
        0x2c => 0x31, // Space
        0x2d => 0x1B, // -
        0x2e => 0x18, // =
        0x2f => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // Caps Lock
        0x3a..=0x45 => FUNCTIONS[(usage - 0x3a) as usize],
        0x49 => 0x72, // Insert maps to Help on a Mac keyboard
        0x4a => 0x73, // Home
        0x4b => 0x74, // Page Up
        0x4c => 0x75, // Forward Delete
        0x4d => 0x77, // End
        0x4e => 0x79, // Page Down
        0x4f => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up
        0xe0 => 0x3B, // Left Control
        0xe1 => 0x38, // Left Shift
        0xe2 => 0x3A, // Left Option
        0xe3 => 0x37, // Left Command
        0xe4 => 0x3E, // Right Control
        0xe5 => 0x3C, // Right Shift
        0xe6 => 0x3D, // Right Option
        0xe7 => 0x36, // Right Command
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use waystone_proto::input::keys;

    #[test]
    fn maps_the_keys_presets_use() {
        assert_eq!(virtual_key(keys::A), Some(0x00));
        assert_eq!(virtual_key(keys::letter('w').unwrap()), Some(0x0D));
        assert_eq!(virtual_key(keys::letter('s').unwrap()), Some(0x01));
        assert_eq!(virtual_key(keys::letter('d').unwrap()), Some(0x02));
        assert_eq!(virtual_key(keys::letter('f').unwrap()), Some(0x03));
        assert_eq!(virtual_key(keys::letter('m').unwrap()), Some(0x2E));
        assert_eq!(virtual_key(keys::Z), Some(0x06));
        assert_eq!(virtual_key(keys::DIGIT_1), Some(0x12));
        assert_eq!(virtual_key(keys::DIGIT_0), Some(0x1D));
        assert_eq!(virtual_key(keys::SPACE), Some(0x31));
        assert_eq!(virtual_key(keys::ENTER), Some(0x24));
        assert_eq!(virtual_key(keys::ESCAPE), Some(0x35));
        assert_eq!(virtual_key(keys::F1), Some(0x7A));
        assert_eq!(virtual_key(keys::F12), Some(0x6F));
        assert_eq!(virtual_key(keys::LEFT_CONTROL), Some(0x3B));
        assert_eq!(virtual_key(keys::UP), Some(0x7E));
        assert_eq!(virtual_key(0xff), None);
    }
}
