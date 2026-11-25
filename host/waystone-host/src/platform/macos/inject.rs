//! Posts decided input actions as CoreGraphics events.

use tracing::debug;

use super::{ffi::*, keymap};
use crate::input::Action;

pub fn post(action: &Action) {
    unsafe {
        match action {
            Action::Move {
                x,
                y,
                held,
                modifiers,
            } => ws_inject_mouse_move(*x, *y, held.map_or(-1, i32::from), *modifiers),
            Action::Button {
                button,
                down,
                x,
                y,
                clicks,
                modifiers,
            } => ws_inject_mouse_button(i32::from(*button), *down, *x, *y, *clicks, *modifiers),
            Action::Scroll { dx, dy } => ws_inject_scroll(*dx, *dy),
            Action::Key {
                usage,
                down,
                modifiers,
                ..
            } => match keymap::virtual_key(*usage) {
                // Modifier flags ride on the event, so implied ones need no
                // separate presses here.
                Some(virtual_key) => ws_inject_key(virtual_key, *down, *modifiers),
                None => debug!("no Mac key for HID usage {usage:#04x}"),
            },
            Action::Text(text) => ws_inject_text(text.as_ptr(), text.len()),
            Action::Wait(duration) => std::thread::sleep(*duration),
        }
    }
}
