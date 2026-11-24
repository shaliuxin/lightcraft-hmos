//! Translate the platform's JSON input events into egui `Event`s and maintain pointer state.
//!
//! The C++ bridge emits compact JSON (touch points, pen/mouse pointer, hardware keys, wheel /
//! pinch, and focus blur). This module folds them into egui 0.37 events, tracking which pointer
//! button is held and the last emitted position so a follow-up move anchors on a real coordinate.
use egui::{
    Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2, TouchPhase, Vec2,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// After a touch ends, ignore a primary re-reported at the same lift point before a new down.
const LEFTOVER_GUARD: Duration = Duration::from_millis(120);

#[derive(Deserialize, Default)]
pub struct Contact {
    pub id: u64,
    pub x: f32,
    pub y: f32,
}

#[derive(Deserialize, Default)]
pub struct Input {
    pub kind: String,
    #[serde(default)]
    pub points: Vec<Contact>,
    #[serde(default)]
    pub action: i32,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub button: i32,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub zoom: f32,
    #[serde(default)]
    pub dx: f32,
    #[serde(default)]
    pub dy: f32,
    /// Set by the platform when pushing a live system-theme token (kind == "system_theme").
    #[serde(default)]
    pub theme: String,
}

#[derive(Default)]
pub struct State {
    pressed: Option<PointerButton>,
    last_pointer: Pos2,
    last_lift: Option<Instant>,
    touches: BTreeMap<u64, Pos2>,
    touch_primary: Option<u64>,
    pending: Vec<Event>,
}

fn button_for(i: i32) -> PointerButton {
    match i {
        1 => PointerButton::Middle,
        2 => PointerButton::Extra1,
        _ => PointerButton::Primary,
    }
}

impl State {
    /// Drain and return the events queued so far (called once per frame before the UI pass).
    pub fn take_pending(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending)
    }

    fn release(&mut self, pos: Pos2, button: PointerButton) {
        self.pending.push(Event::PointerButton {
            pos,
            button,
            pressed: false,
            modifiers: Default::default(),
        });
    }

    pub fn apply(&mut self, input: &Input) {
        match input.kind.as_str() {
            "touch" => {
                let now: BTreeMap<u64, Pos2> =
                    input.points.iter().map(|p| (p.id, Pos2::new(p.x, p.y))).collect();
                for (id, pos) in now.iter() {
                    if !self.touches.contains_key(id) {
                        if self.touch_primary.is_none() {
                            self.touch_primary = Some(*id);
                            self.pending.push(Event::PointerButton {
                                pos: *pos,
                                button: PointerButton::Primary,
                                pressed: true,
                                modifiers: Default::default(),
                            });
                            self.pressed = Some(PointerButton::Primary);
                            self.last_pointer = *pos;
                        }
                    } else if Some(*id) == self.touch_primary {
                        self.last_pointer = *pos;
                        self.pending.push(Event::PointerMoved(*pos));
                    }
                }
                let gone: Vec<u64> =
                    self.touches.keys().copied().filter(|id| !now.contains_key(id)).collect();
                for id in gone {
                    self.touches.remove(&id);
                    if Some(id) == self.touch_primary {
                        self.release(self.last_pointer, PointerButton::Primary);
                        self.pressed = None;
                    }
                }
                self.touch_primary = now.keys().next().copied();
                self.touches = now;
            }
            "pointer" => {
                let pos = Pos2::new(input.x, input.y);
                let button = button_for(input.button);
                match input.action {
                    0 => {
                        self.last_lift = None;
                        if Some(button) == self.pressed {
                            self.pending.push(Event::PointerMoved(pos));
                        } else {
                            self.pending.push(Event::PointerButton {
                                pos,
                                button,
                                pressed: true,
                                modifiers: Default::default(),
                            });
                            self.pressed = Some(button);
                        }
                        if matches!(button, PointerButton::Primary) {
                            self.last_pointer = pos;
                        }
                    }
                    1 => {
                        self.last_lift = Some(Instant::now());
                        self.release(pos, button);
                        if Some(button) == self.pressed {
                            self.pressed = None;
                        }
                    }
                    3 => {
                        if let Some(button) = self.pressed {
                            self.release(self.last_pointer, button);
                        }
                        self.pressed = None;
                    }
                    _ => {
                        let fresh = self
                            .last_lift
                            .map(|t| t.elapsed() > LEFTOVER_GUARD)
                            .unwrap_or(true);
                        if fresh {
                            self.last_pointer = pos;
                            self.pending.push(Event::PointerMoved(pos));
                        }
                    }
                }
            }
            "key" => {
                let pressed = input.action == 0;
                let mods = Modifiers {
                    ctrl: input.ctrl,
                    shift: input.shift,
                    alt: input.alt,
                    ..Default::default()
                };
                if pressed && !input.text.is_empty() {
                    for ch in input.text.chars() {
                        self.pending.push(Event::Text(ch.to_string()));
                    }
                }
                if let Some(key) = map_key(input.code) {
                    self.pending.push(Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: mods,
                    });
                }
            }
            "axis" => {
                if input.zoom > 0.0 {
                    self.pending.push(Event::Zoom((input.zoom - 1.0) * 10.0));
                }
                if input.dx != 0.0 || input.dy != 0.0 {
                    self.pending.push(Event::MouseWheel {
                        unit: MouseWheelUnit::Line,
                        delta: Vec2::new(input.dx, input.dy),
                        phase: TouchPhase::Move,
                        source: MouseWheelSource::Unknown,
                        modifiers: Default::default(),
                    });
                }
            }
            "blur" => {
                if let Some(button) = self.pressed {
                    self.release(self.last_pointer, button);
                    self.pressed = None;
                }
                self.touches.clear();
                self.touch_primary = None;
            }
            _ => {}
        }
    }
}

/// Map HarmonyOS `OH_NativeXComponent_KeyCode` values to egui keys (letters/digits arrive as text).
fn map_key(code: i32) -> Option<Key> {
    Some(match code {
        2050 => Key::Space,
        2043 => Key::Comma,
        2044 => Key::Period,
        2051 => Key::Escape,
        2052 => Key::Tab,
        2053 => Key::Backspace,
        2054 => Key::Enter,
        2055 => Key::A,
        2064 => Key::L,
        2070 => Key::Z,
        2071 => Key::X,
        2074 => Key::ArrowUp,
        2075 => Key::ArrowLeft,
        2076 => Key::ArrowRight,
        2077 => Key::ArrowDown,
        2083 => Key::F1,
        2084 => Key::F2,
        2085 => Key::F3,
        2086 => Key::F4,
        2087 => Key::F5,
        2088 => Key::F6,
        2089 => Key::F7,
        2090 => Key::F8,
        2091 => Key::F9,
        2092 => Key::F10,
        2093 => Key::F11,
        2094 => Key::F12,
        _ => return None,
    })
}
