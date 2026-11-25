//! Input from the controller, injected into the local desktop.
//!
//! Frames are decoded and interpreted here; the Swift layer only posts the
//! resulting CoreGraphics events. Interpretation is a pure state machine
//! ([`InputState`]) so the rules are testable without a desktop:
//!
//! * every held key and button is tracked, so [`InputState::release_all`]
//!   (on `ReleaseAll`, and whenever a session ends for any reason) lets go of
//!   exactly what is down: a dropped link must never leave Ctrl held;
//! * a button released less than [`MIN_CLICK_HOLD`] after it went down waits
//!   for the rest of that time, because games that poll the button state miss
//!   a press shorter than a frame (design doc 5.2);
//! * consecutive clicks at the same spot carry a click count, so a double
//!   click on the phone is a double click on the Mac;
//! * held modifier keys apply to every mouse and key event.

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use waystone_proto::input::{modifiers as modifier_bits, InputEvent};

use crate::display::Display;
use crate::platform;

/// Shortest time a button stays down.
pub const MIN_CLICK_HOLD: Duration = Duration::from_millis(15);
/// Clicks closer than this in time and space count as a multi-click.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 6.0;

/// One thing to post, in display points.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Move {
        x: f64,
        y: f64,
        held: Option<u8>,
        modifiers: u8,
    },
    Button {
        button: u8,
        down: bool,
        x: f64,
        y: f64,
        clicks: i32,
        modifiers: u8,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        /// USB HID usage; each platform maps it to its own key codes.
        usage: u32,
        down: bool,
        /// Every modifier in effect, held or requested.
        modifiers: u8,
        /// Requested by the event but not held as keys. Platforms whose key
        /// events cannot carry modifier flags press these around the key.
        implied: u8,
    },
    Text(String),
    Wait(Duration),
}

/// Interprets input events against what is currently held.
pub struct InputState {
    display: Display,
    cursor: (f64, f64),
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u8>,
    pressed_at: [Option<Instant>; 5],
    last_click: Option<(Instant, (f64, f64), i32)>,
}

impl InputState {
    pub fn new(display: Display) -> InputState {
        InputState {
            display,
            cursor: display.point(u16::MAX / 2, u16::MAX / 2),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            pressed_at: [None; 5],
            last_click: None,
        }
    }

    /// Modifier bits from the modifier keys currently held.
    fn held_modifiers(&self) -> u8 {
        self.keys
            .iter()
            .filter_map(|&key| modifier_bit(key))
            .fold(0, |bits, bit| bits | bit)
    }

    pub fn handle(&mut self, event: InputEvent, now: Instant) -> Vec<Action> {
        match event {
            InputEvent::MouseMove { x, y } => {
                self.cursor = self.display.point(x, y);
                vec![Action::Move {
                    x: self.cursor.0,
                    y: self.cursor.1,
                    held: self.buttons.iter().next().copied(),
                    modifiers: self.held_modifiers(),
                }]
            }
            InputEvent::MouseButton { button, down, x, y } => {
                self.cursor = self.display.point(x, y);
                self.button(button as u8, down, now)
            }
            InputEvent::MouseWheel { dx, dy } => vec![Action::Scroll {
                dx: i32::from(dx),
                dy: i32::from(dy),
            }],
            InputEvent::Key {
                code,
                down,
                modifiers,
            } => {
                if down {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
                let held = self.held_modifiers();
                vec![Action::Key {
                    usage: code,
                    down,
                    modifiers: modifiers | held,
                    implied: modifiers & !held,
                }]
            }
            InputEvent::Text(text) => vec![Action::Text(text)],
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::Pad(_) => {
                debug!("gamepad input needs a virtual controller driver; ignored");
                Vec::new()
            }
        }
    }

    fn button(&mut self, button: u8, down: bool, now: Instant) -> Vec<Action> {
        let (x, y) = self.cursor;
        let modifiers = self.held_modifiers();
        let slot = usize::from(button.min(4));
        let mut actions = Vec::new();
        if down {
            if !self.buttons.insert(button) {
                return actions;
            }
            self.pressed_at[slot] = Some(now);
            let clicks = match self.last_click {
                Some((at, (cx, cy), count))
                    if now.duration_since(at) < MULTI_CLICK_INTERVAL
                        && (cx - x).hypot(cy - y) < MULTI_CLICK_DISTANCE =>
                {
                    count + 1
                }
                _ => 1,
            };
            self.last_click = Some((now, (x, y), clicks));
            actions.push(Action::Button {
                button,
                down,
                x,
                y,
                clicks,
                modifiers,
            });
        } else {
            if !self.buttons.remove(&button) {
                return actions;
            }
            if let Some(pressed) = self.pressed_at[slot].take() {
                let held = now.saturating_duration_since(pressed);
                if held < MIN_CLICK_HOLD {
                    actions.push(Action::Wait(MIN_CLICK_HOLD - held));
                }
            }
            let clicks = self.last_click.map(|(_, _, count)| count).unwrap_or(1);
            actions.push(Action::Button {
                button,
                down,
                x,
                y,
                clicks,
                modifiers,
            });
        }
        actions
    }

    /// Lets go of every held button and key, modifiers last.
    pub fn release_all(&mut self) -> Vec<Action> {
        let (x, y) = self.cursor;
        let mut actions: Vec<Action> = std::mem::take(&mut self.buttons)
            .into_iter()
            .map(|button| Action::Button {
                button,
                down: false,
                x,
                y,
                clicks: 1,
                modifiers: 0,
            })
            .collect();
        self.pressed_at = [None; 5];
        let keys = std::mem::take(&mut self.keys);
        let (modifiers, plain): (Vec<u32>, Vec<u32>) = keys
            .into_iter()
            .partition(|&key| modifier_bit(key).is_some());
        for usage in plain.into_iter().chain(modifiers) {
            actions.push(Action::Key {
                usage,
                down: false,
                modifiers: 0,
                implied: 0,
            });
        }
        actions
    }
}

/// The modifier bit a HID usage stands for, if it is a modifier key.
pub fn modifier_bit(usage: u32) -> Option<u8> {
    match usage {
        0xe0 | 0xe4 => Some(modifier_bits::CONTROL),
        0xe1 | 0xe5 => Some(modifier_bits::SHIFT),
        0xe2 | 0xe6 => Some(modifier_bits::ALT),
        0xe3 | 0xe7 => Some(modifier_bits::META),
        _ => None,
    }
}

enum Command {
    Frame(Vec<u8>),
    ReleaseAll,
    Stop,
}

/// Queues raw input frames for the injector thread.
#[derive(Clone)]
pub struct InputSender(mpsc::Sender<Command>);

impl InputSender {
    pub fn submit(&self, frame: Vec<u8>) {
        let _ = self.0.send(Command::Frame(frame));
    }
}

/// Injects on its own thread, so click hold times never stall the network.
///
/// Dropping the injector releases everything still held and joins the thread.
pub struct Injector {
    commands: Option<mpsc::Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl Injector {
    pub fn start(display: Display) -> Injector {
        let (commands, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("waystone-input".into())
            .spawn(move || run(display, receiver))
            .expect("spawning the input thread");
        Injector {
            commands: Some(commands),
            thread: Some(thread),
        }
    }

    /// A cheap handle for queueing frames from the engine's callback thread.
    pub fn sender(&self) -> InputSender {
        InputSender(self.commands.clone().expect("the injector is running"))
    }

    pub fn release_all(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::ReleaseAll);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
        // Senders held by the engine may outlive this, so the thread is told
        // to stop rather than waiting for the channel to close.
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Command::Stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(display: Display, commands: mpsc::Receiver<Command>) {
    let mut state = InputState::new(display);
    for command in commands {
        let actions = match command {
            Command::Frame(frame) => match InputEvent::decode(&frame) {
                Ok(event) => {
                    debug!("input {event:?}");
                    state.handle(event, Instant::now())
                }
                Err(error) => {
                    warn!("dropping input frame: {error}");
                    continue;
                }
            },
            Command::ReleaseAll => state.release_all(),
            Command::Stop => break,
        };
        actions.iter().for_each(platform::post);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waystone_proto::input::{keys, modifiers, MouseButton};

    fn display() -> Display {
        Display {
            id: 1,
            origin: (0.0, 0.0),
            size: (1000.0, 500.0),
            pixels: (2000, 1000),
        }
    }

    fn click(button: MouseButton, down: bool) -> InputEvent {
        InputEvent::MouseButton {
            button,
            down,
            x: u16::MAX,
            y: 0,
        }
    }

    #[test]
    fn a_quick_click_is_held_for_a_frame() {
        let mut state = InputState::new(display());
        let start = Instant::now();
        let down = state.handle(click(MouseButton::Left, true), start);
        assert!(matches!(
            down[0],
            Action::Button {
                down: true,
                x,
                clicks: 1,
                ..
            } if x == 1000.0
        ));
        let up = state.handle(
            click(MouseButton::Left, false),
            start + Duration::from_millis(1),
        );
        assert_eq!(up[0], Action::Wait(Duration::from_millis(14)));
        assert!(matches!(up[1], Action::Button { down: false, .. }));
    }

    #[test]
    fn two_quick_clicks_are_a_double_click() {
        let mut state = InputState::new(display());
        let start = Instant::now();
        state.handle(click(MouseButton::Left, true), start);
        state.handle(
            click(MouseButton::Left, false),
            start + Duration::from_millis(30),
        );
        let second = state.handle(
            click(MouseButton::Left, true),
            start + Duration::from_millis(200),
        );
        assert!(matches!(second[0], Action::Button { clicks: 2, .. }));
    }

    #[test]
    fn moving_with_a_button_down_is_a_drag() {
        let mut state = InputState::new(display());
        state.handle(click(MouseButton::Left, true), Instant::now());
        let actions = state.handle(InputEvent::MouseMove { x: 0, y: 0 }, Instant::now());
        assert!(matches!(actions[0], Action::Move { held: Some(0), .. }));
    }

    #[test]
    fn held_control_applies_to_clicks_and_release_all_lets_go_of_everything() {
        let mut state = InputState::new(display());
        let now = Instant::now();
        state.handle(
            InputEvent::Key {
                code: keys::LEFT_CONTROL,
                down: true,
                modifiers: 0,
            },
            now,
        );
        state.handle(
            InputEvent::Key {
                code: keys::letter('w').unwrap(),
                down: true,
                modifiers: 0,
            },
            now,
        );
        let down = state.handle(click(MouseButton::Right, true), now);
        assert!(matches!(down[0], Action::Button { modifiers: m, .. } if m == modifiers::CONTROL));

        let released = state.release_all();
        assert_eq!(
            released,
            vec![
                Action::Button {
                    button: 1,
                    down: false,
                    x: 1000.0,
                    y: 0.0,
                    clicks: 1,
                    modifiers: 0
                },
                // W before Control, so nothing is released into a shortcut.
                Action::Key {
                    usage: keys::letter('w').unwrap(),
                    down: false,
                    modifiers: 0,
                    implied: 0
                },
                Action::Key {
                    usage: keys::LEFT_CONTROL,
                    down: false,
                    modifiers: 0,
                    implied: 0
                },
            ]
        );
        assert!(state.release_all().is_empty());
    }

    #[test]
    fn requested_modifiers_that_are_not_held_are_implied() {
        let mut state = InputState::new(display());
        let now = Instant::now();
        let key = |code, down, modifiers| InputEvent::Key {
            code,
            down,
            modifiers,
        };
        let copy = state.handle(
            key(keys::letter('c').unwrap(), true, modifiers::CONTROL),
            now,
        );
        assert!(matches!(
            copy[0],
            Action::Key { modifiers: m, implied: i, .. } if m == modifiers::CONTROL && i == modifiers::CONTROL
        ));
        state.handle(key(keys::LEFT_SHIFT, true, 0), now);
        let held = state.handle(key(keys::A, true, modifiers::SHIFT), now);
        assert!(matches!(held[0], Action::Key { implied: 0, .. }));
    }

    #[test]
    fn knows_modifier_keys() {
        assert_eq!(modifier_bit(keys::LEFT_CONTROL), Some(modifiers::CONTROL));
        assert_eq!(modifier_bit(keys::LEFT_META), Some(modifiers::META));
        assert_eq!(modifier_bit(keys::A), None);
    }

    #[test]
    fn a_stray_release_does_nothing() {
        let mut state = InputState::new(display());
        assert!(state
            .handle(click(MouseButton::Left, false), Instant::now())
            .is_empty());
    }
}
