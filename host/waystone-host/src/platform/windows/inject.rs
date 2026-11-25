//! Posts decided input actions with `SendInput`.
//!
//! Pointer positions arrive in virtual desktop pixels and are sent as
//! absolute coordinates normalized over the whole virtual desktop, so every
//! monitor layout works. Keys go as scan codes (see `keymap`).

use std::cell::Cell;
use std::mem::size_of;

use tracing::{debug, info};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::desktop::{self, SwitchOutcome};
use super::keymap::{scan_code, ScanCode};
use crate::input::Action;

thread_local! {
    /// Tracks the last desktop generation this input thread synced to.
    static SEEN_DESKTOP_GEN: Cell<u64> = const { Cell::new(u64::MAX) };
}

fn ensure_input_desktop() {
    let gen = desktop::desktop_generation();
    let stale = SEEN_DESKTOP_GEN.with(|cell| {
        if cell.get() != gen {
            cell.set(gen);
            true
        } else {
            false
        }
    });
    if stale {
        if let SwitchOutcome::Switched(name) = desktop::sync_thread_to_input_desktop() {
            info!("input thread switched to desktop {name}");
        }
    }
}

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
/// One wheel notch scrolls about three lines, roughly 48 pixels.
const WHEEL_PER_PIXEL: f64 = 120.0 / 48.0;

/// The modifier keys behind each protocol modifier bit: Shift, Control,
/// Alt, Windows.
const MODIFIER_KEYS: [(u8, u32); 4] = [(0x01, 0xe1), (0x02, 0xe0), (0x04, 0xe2), (0x08, 0xe3)];

pub fn post(action: &Action) {
    ensure_input_desktop();
    match action {
        Action::Move { x, y, .. } => send(&[mouse(Some((*x, *y)), 0, MOUSEEVENTF_MOVE)]),
        Action::Button {
            button, down, x, y, ..
        } => {
            // Windows counts double clicks itself from timing and distance.
            let (flags, data) = button_flags(*button, *down);
            send(&[mouse(Some((*x, *y)), data, MOUSEEVENTF_MOVE | flags)]);
        }
        Action::Scroll { dx, dy } => {
            let mut inputs = Vec::new();
            if *dy != 0 {
                // Positive means up on both sides.
                let amount = (f64::from(*dy) * WHEEL_PER_PIXEL).round() as i32;
                inputs.push(mouse(None, amount as u32, MOUSEEVENTF_WHEEL));
            }
            if *dx != 0 {
                // The protocol's positive horizontal delta scrolls left; the
                // horizontal wheel's positive direction is right.
                let amount = (-f64::from(*dx) * WHEEL_PER_PIXEL).round() as i32;
                inputs.push(mouse(None, amount as u32, MOUSEEVENTF_HWHEEL));
            }
            send(&inputs);
        }
        Action::Key {
            usage,
            down,
            implied,
            ..
        } => {
            let Some(code) = scan_code(*usage) else {
                debug!("no scan code for HID usage {usage:#04x}");
                return;
            };
            // Modifiers asked for but not held are pressed around a key
            // press only: a key release needs no modifiers.
            let around: Vec<ScanCode> = if *down {
                MODIFIER_KEYS
                    .iter()
                    .filter(|(bit, _)| implied & bit != 0)
                    .filter_map(|(_, usage)| scan_code(*usage))
                    .collect()
            } else {
                Vec::new()
            };
            let mut inputs: Vec<INPUT> = around.iter().map(|m| key(*m, true)).collect();
            inputs.push(key(code, *down));
            inputs.extend(around.iter().rev().map(|m| key(*m, false)));
            send(&inputs);
        }
        Action::Text(text) => {
            let inputs: Vec<INPUT> = text
                .encode_utf16()
                .flat_map(|unit| [unicode(unit, true), unicode(unit, false)])
                .collect();
            send(&inputs);
        }
        Action::Wait(duration) => std::thread::sleep(*duration),
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    let mut sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // A desktop transition (UAC / Winlogon) may have occurred before the
        // watcher observed it; re-attach the input thread and retry once.
        if let SwitchOutcome::Switched(name) = desktop::sync_thread_to_input_desktop() {
            info!("input thread re-attached to desktop {name}, retrying SendInput");
            sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        }
    }
    if sent as usize != inputs.len() {
        // Blocked by UIPI (an elevated window has focus) or the desktop is
        // not ours (lock screen, UAC prompt).
        debug!("SendInput delivered {sent} of {} events", inputs.len());
    }
}

fn button_flags(button: u8, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (button, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (_, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (_, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    }
}

/// A mouse input, absolute over the virtual desktop when `at` is given.
fn mouse(at: Option<(f64, f64)>, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    let (dx, dy, flags) = match at {
        Some((x, y)) => {
            let (nx, ny) = normalize(x, y);
            (
                nx,
                ny,
                flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )
        }
        None => (0, 0, flags),
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Virtual desktop pixels to the `0..=65535` range absolute input uses.
fn normalize(x: f64, y: f64) -> (i32, i32) {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let scale = |value: f64, origin: i32, extent: i32| {
        ((value - f64::from(origin)) * 65535.0 / f64::from(extent - 1))
            .round()
            .clamp(0.0, 65535.0) as i32
    };
    (scale(x, left, width), scale(y, top, height))
}

fn key(code: ScanCode, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if code.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    keyboard(code.code, flags)
}

fn unicode(unit: u16, down: bool) -> INPUT {
    let flags = if down {
        KEYEVENTF_UNICODE
    } else {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    };
    keyboard(unit, flags)
}

fn keyboard(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
