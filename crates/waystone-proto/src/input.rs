//! Low level input frames sent from a controller to a host.
//!
//! Two data channels are used:
//!
//! * `input-fast` – unreliable and unordered, carries only [`InputEvent::MouseMove`]
//!   and [`InputEvent::Pad`] samples. Losing an old sample is harmless because the
//!   next one supersedes it.
//! * `input-reliable` – reliable and ordered, carries everything that is a state
//!   edge: button presses, keys, wheel ticks, clipboard text.
//!
//! Every frame is a single byte opcode followed by a fixed layout. Keeping the
//! encoding hand rolled (instead of a generic serialization framework) makes the
//! wire format auditable from Swift and Rust alike.

use std::fmt;

/// Coordinate space used by mouse positions: the host maps `0..=65535` onto the
/// whole virtual desktop, which is resolution independent.
pub const AXIS_MAX: u16 = u16::MAX;

/// Opcodes on the wire.
mod opcode {
    pub const MOUSE_MOVE: u8 = 0x01;
    pub const MOUSE_BUTTON: u8 = 0x02;
    pub const MOUSE_WHEEL: u8 = 0x03;
    pub const KEY: u8 = 0x04;
    pub const TEXT: u8 = 0x05;
    pub const RELEASE_ALL: u8 = 0x06;
    pub const PAD: u8 = 0x07;
}

/// One of the classic three mouse buttons plus the two extended ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

impl MouseButton {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::Back,
            4 => MouseButton::Forward,
            _ => return None,
        })
    }
}

/// A synthetic stick / trigger sample, used by the on-screen gamepad preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadState {
    /// Left stick, `-32768..=32767`.
    pub lx: i16,
    pub ly: i16,
    /// Right stick, `-32768..=32767`.
    pub rx: i16,
    pub ry: i16,
    /// Bit mask of pressed buttons, see [`PadButton`].
    pub buttons: u32,
}

/// Pad button bit indices, matching a standard XInput layout order.
pub struct PadButton;

impl PadButton {
    pub const A: u32 = 1 << 0;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const LB: u32 = 1 << 4;
    pub const RB: u32 = 1 << 5;
    pub const BACK: u32 = 1 << 6;
    pub const START: u32 = 1 << 7;
}

/// Keyboard modifier bit mask shared by hosts and controllers.
pub mod modifiers {
    pub const SHIFT: u8 = 1 << 0;
    pub const CONTROL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const META: u8 = 1 << 3;
}

/// A decoded input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Absolute cursor position, normalized to `0..=65535`.
    MouseMove { x: u16, y: u16 },
    /// Button edge with the position snapshot taken at press/release time so the
    /// click cannot land somewhere else if a move is still in flight.
    MouseButton {
        button: MouseButton,
        down: bool,
        x: u16,
        y: u16,
    },
    MouseWheel { dx: i16, dy: i16 },
    Key {
        code: u32,
        down: bool,
        modifiers: u8,
    },
    /// Unicode text, used for soft keyboards and clipboard paste.
    Text(String),
    /// Release every key and button the host thinks is held down.
    /// Sent on disconnect and on gesture preset changes.
    ReleaseAll,
    /// Virtual gamepad sample.
    Pad(PadState),
}

/// Errors produced while decoding an input frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    Empty,
    UnknownOpcode(u8),
    Truncated { opcode: u8, needed: usize, got: usize },
    InvalidUtf8,
    UnknownButton(u8),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::Empty => f.write_str("empty input frame"),
            InputError::UnknownOpcode(op) => write!(f, "unknown input opcode {op:#04x}"),
            InputError::Truncated {
                opcode,
                needed,
                got,
            } => write!(f, "input opcode {opcode:#04x} needs {needed} bytes, got {got}"),
            InputError::InvalidUtf8 => f.write_str("input text is not valid UTF-8"),
            InputError::UnknownButton(b) => write!(f, "unknown mouse button {b}"),
        }
    }
}

impl std::error::Error for InputError {}

impl InputEvent {
    /// True when the event must travel on the reliable channel.
    pub fn is_reliable(&self) -> bool {
        match self {
            InputEvent::MouseMove { .. } | InputEvent::Pad(_) => false,
            _ => true,
        }
    }

    /// Serializes the event into `out`. Returns the number of bytes written.
    pub fn encode(&self, out: &mut Vec<u8>) -> usize {
        let start = out.len();
        match self {
            InputEvent::MouseMove { x, y } => {
                out.push(opcode::MOUSE_MOVE);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            InputEvent::MouseButton {
                button,
                down,
                x,
                y,
            } => {
                out.push(opcode::MOUSE_BUTTON);
                out.push(*button as u8);
                out.push(u8::from(*down));
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            InputEvent::MouseWheel { dx, dy } => {
                out.push(opcode::MOUSE_WHEEL);
                out.extend_from_slice(&dx.to_be_bytes());
                out.extend_from_slice(&dy.to_be_bytes());
            }
            InputEvent::Key {
                code,
                down,
                modifiers,
            } => {
                out.push(opcode::KEY);
                out.extend_from_slice(&code.to_be_bytes());
                out.push(u8::from(*down));
                out.push(*modifiers);
            }
            InputEvent::Text(text) => {
                out.push(opcode::TEXT);
                out.extend_from_slice(text.as_bytes());
            }
            InputEvent::ReleaseAll => {
                out.push(opcode::RELEASE_ALL);
            }
            InputEvent::Pad(pad) => {
                out.push(opcode::PAD);
                out.extend_from_slice(&pad.lx.to_be_bytes());
                out.extend_from_slice(&pad.ly.to_be_bytes());
                out.extend_from_slice(&pad.rx.to_be_bytes());
                out.extend_from_slice(&pad.ry.to_be_bytes());
                out.extend_from_slice(&pad.buttons.to_be_bytes());
            }
        }
        out.len() - start
    }

    /// Convenience wrapper around [`InputEvent::encode`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        self.encode(&mut buf);
        buf
    }

    /// Decodes a single frame.
    pub fn decode(bytes: &[u8]) -> Result<Self, InputError> {
        let (&code, rest) = bytes.split_first().ok_or(InputError::Empty)?;
        let need = |n: usize| -> Result<(), InputError> {
            if rest.len() < n {
                Err(InputError::Truncated {
                    opcode: code,
                    needed: n,
                    got: rest.len(),
                })
            } else {
                Ok(())
            }
        };

        match code {
            opcode::MOUSE_MOVE => {
                need(4)?;
                Ok(InputEvent::MouseMove {
                    x: u16::from_be_bytes([rest[0], rest[1]]),
                    y: u16::from_be_bytes([rest[2], rest[3]]),
                })
            }
            opcode::MOUSE_BUTTON => {
                need(6)?;
                let button = MouseButton::from_u8(rest[0]).ok_or(InputError::UnknownButton(rest[0]))?;
                Ok(InputEvent::MouseButton {
                    button,
                    down: rest[1] != 0,
                    x: u16::from_be_bytes([rest[2], rest[3]]),
                    y: u16::from_be_bytes([rest[4], rest[5]]),
                })
            }
            opcode::MOUSE_WHEEL => {
                need(4)?;
                Ok(InputEvent::MouseWheel {
                    dx: i16::from_be_bytes([rest[0], rest[1]]),
                    dy: i16::from_be_bytes([rest[2], rest[3]]),
                })
            }
            opcode::KEY => {
                need(6)?;
                Ok(InputEvent::Key {
                    code: u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]),
                    down: rest[4] != 0,
                    modifiers: rest[5],
                })
            }
            opcode::TEXT => {
                let text = std::str::from_utf8(rest).map_err(|_| InputError::InvalidUtf8)?;
                Ok(InputEvent::Text(text.to_string()))
            }
            opcode::RELEASE_ALL => Ok(InputEvent::ReleaseAll),
            opcode::PAD => {
                need(12)?;
                Ok(InputEvent::Pad(PadState {
                    lx: i16::from_be_bytes([rest[0], rest[1]]),
                    ly: i16::from_be_bytes([rest[2], rest[3]]),
                    rx: i16::from_be_bytes([rest[4], rest[5]]),
                    ry: i16::from_be_bytes([rest[6], rest[7]]),
                    buttons: u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]),
                }))
            }
            other => Err(InputError::UnknownOpcode(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: InputEvent) {
        let bytes = event.to_bytes();
        assert_eq!(InputEvent::decode(&bytes), Ok(event));
    }

    #[test]
    fn roundtrips_every_variant() {
        roundtrip(InputEvent::MouseMove {
            x: 1234,
            y: AXIS_MAX,
        });
        roundtrip(InputEvent::MouseButton {
            button: MouseButton::Right,
            down: true,
            x: 10,
            y: 20,
        });
        roundtrip(InputEvent::MouseWheel { dx: -3, dy: 120 });
        roundtrip(InputEvent::Key {
            code: 0x2c,
            down: true,
            modifiers: modifiers::CONTROL,
        });
        roundtrip(InputEvent::Text("hello ✓".into()));
        roundtrip(InputEvent::ReleaseAll);
        roundtrip(InputEvent::Pad(PadState {
            lx: -32768,
            ly: 32767,
            rx: 0,
            ry: -1,
            buttons: PadButton::A | PadButton::START,
        }));
    }

    #[test]
    fn reliable_classification() {
        assert!(!InputEvent::MouseMove { x: 0, y: 0 }.is_reliable());
        assert!(InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
            x: 0,
            y: 0
        }
        .is_reliable());
        assert!(InputEvent::ReleaseAll.is_reliable());
    }

    #[test]
    fn rejects_truncated_and_unknown_frames() {
        assert_eq!(InputEvent::decode(&[]), Err(InputError::Empty));
        assert_eq!(
            InputEvent::decode(&[opcode::MOUSE_MOVE, 0, 1]),
            Err(InputError::Truncated {
                opcode: opcode::MOUSE_MOVE,
                needed: 4,
                got: 2
            })
        );
        assert_eq!(
            InputEvent::decode(&[0x7f]),
            Err(InputError::UnknownOpcode(0x7f))
        );
    }
}
