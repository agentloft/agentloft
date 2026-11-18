//! Touches for a phone, as a touch screen of our own: Android's `uinput`
//! shell command (part of the system since Android 11, there for its own
//! tests) makes a virtual input device from a description and replays the
//! Linux input events it is sent, one JSON object a line on its stdin. The
//! system takes the device for a second touch screen on the built-in
//! display, so a finger moves as smoothly as it is sent and several fingers
//! (multitouch slots) work. Nothing is installed on the phone.
//!
//! Writing to the phone's own `/dev/input/event*` (`sendevent`) is refused
//! to adb's shell by SELinux on production (`user`) builds; `/dev/uinput`
//! isn't, since the shell is in its group for these commands.

use serde_json::{Value, json};

// Linux input event types and codes (linux/input-event-codes.h).
const EV_SYN: i32 = 0;
const EV_KEY: i32 = 1;
const EV_ABS: i32 = 3;
const SYN_REPORT: i32 = 0;
const BTN_TOUCH: i32 = 0x14a;
const BTN_TOOL_FINGER: i32 = 0x145;
const ABS_MT_SLOT: i32 = 0x2f;
const ABS_MT_TOUCH_MAJOR: i32 = 0x30;
const ABS_MT_POSITION_X: i32 = 0x35;
const ABS_MT_POSITION_Y: i32 = 0x36;
const ABS_MT_TRACKING_ID: i32 = 0x39;
const ABS_MT_PRESSURE: i32 = 0x3a;
const INPUT_PROP_DIRECT: i32 = 1;
// What `uinput`'s configuration calls the ioctls (their numbers).
const UI_SET_EVBIT: i32 = 100;
const UI_SET_KEYBIT: i32 = 101;
const UI_SET_ABSBIT: i32 = 103;
const UI_SET_PROPBIT: i32 = 110;

/// Fingers the device has (`ABS_MT_SLOT` 0 to SLOTS - 1).
pub const SLOTS: usize = 10;
/// The slot of the finger that scrolls (the others count up from 0).
pub const SCROLL_SLOT: usize = SLOTS - 1;
/// The name the device has on the phone (`dumpsys input` shows it).
pub const DEVICE_NAME: &str = "Agentloft touch";

/// The `register` line: a direct (touch screen) multitouch device whose
/// axes are the display's pixels as built (`width` × `height`), on `bus`
/// (`virtual`; older systems know only `usb` and `bluetooth`).
pub fn register(width: u32, height: u32, bus: &str) -> String {
    let abs = |code: i32, max: i64| {
        json!({"code": code, "info": {
            "value": 0, "minimum": 0, "maximum": max, "fuzz": 0, "flat": 0, "resolution": 0
        }})
    };
    json!({
        "id": 1,
        "command": "register",
        "name": DEVICE_NAME,
        "vid": 0x18d1,
        "pid": 0x4c0f,
        "bus": bus,
        "configuration": [
            {"type": UI_SET_EVBIT, "data": [EV_KEY, EV_ABS]},
            {"type": UI_SET_KEYBIT, "data": [BTN_TOUCH, BTN_TOOL_FINGER]},
            {"type": UI_SET_ABSBIT, "data": [
                ABS_MT_SLOT, ABS_MT_TOUCH_MAJOR, ABS_MT_POSITION_X, ABS_MT_POSITION_Y,
                ABS_MT_TRACKING_ID, ABS_MT_PRESSURE
            ]},
            {"type": UI_SET_PROPBIT, "data": [INPUT_PROP_DIRECT]},
        ],
        "abs_info": [
            abs(ABS_MT_SLOT, SLOTS as i64 - 1),
            abs(ABS_MT_TOUCH_MAJOR, 255),
            abs(ABS_MT_POSITION_X, width.saturating_sub(1).max(1) as i64),
            abs(ABS_MT_POSITION_Y, height.saturating_sub(1).max(1) as i64),
            abs(ABS_MT_TRACKING_ID, 65535),
            abs(ABS_MT_PRESSURE, 255),
        ],
    })
    .to_string()
}

/// An `inject` line for `events` (type, code, value triples).
fn inject(events: &[i32]) -> String {
    let events: Vec<Value> = events.iter().map(|&v| Value::from(v)).collect();
    json!({"id": 1, "command": "inject", "events": events}).to_string()
}

/// How the display is turned (`Surface.ROTATION_*`): quarter turns of the
/// interface clockwise, the device turned the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rotation(pub u8);

impl Rotation {
    pub fn sideways(self) -> bool {
        self.0 % 2 == 1
    }

    /// The screen as the pictures show it, from its size as built.
    pub fn size(self, width: u32, height: u32) -> (u32, u32) {
        if self.sideways() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// A point of the picture (fractions, as the user sees the screen) as
    /// fractions of the display as built, which is how a touch screen
    /// reports (the system turns its points with the display).
    pub fn to_natural(self, x: f32, y: f32) -> (f32, f32) {
        let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        match self.0 % 4 {
            // Turned a quarter: the device's top is on the left, so its top
            // right corner is at the picture's top left.
            1 => (1.0 - y, x),
            2 => (1.0 - x, 1.0 - y),
            3 => (y, 1.0 - x),
            _ => (x, y),
        }
    }
}

/// The fingers on the virtual touch screen, and the events that move them.
#[derive(Debug)]
pub struct Fingers {
    width: u32,
    height: u32,
    /// The tracking id of the finger down in each slot.
    down: [Option<i32>; SLOTS],
    next_id: i32,
}

impl Fingers {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            down: [None; SLOTS],
            next_id: 1,
        }
    }

    pub fn any_down(&self) -> bool {
        self.down.iter().any(Option::is_some)
    }

    pub fn is_down(&self, slot: usize) -> bool {
        self.down.get(slot).is_some_and(Option::is_some)
    }

    fn pixel(&self, nx: f32, ny: f32) -> (i32, i32) {
        let px = |f: f32, n: u32| {
            ((f * n as f32).round() as i32).clamp(0, n.saturating_sub(1).max(1) as i32)
        };
        (px(nx, self.width), px(ny, self.height))
    }

    /// One frame of the touch screen: each `(slot, Some(point))` puts that
    /// finger down or moves it there, each `(slot, None)` lifts it; points
    /// in fractions of the display as built. The `inject` line for it, or
    /// `None` when nothing changes.
    pub fn frame(&mut self, changes: &[(usize, Option<(f32, f32)>)]) -> Option<String> {
        let was_down = self.any_down();
        let mut events = Vec::new();
        for &(slot, at) in changes {
            if slot >= SLOTS {
                continue;
            }
            match (at, self.down[slot]) {
                (Some((nx, ny)), held) => {
                    let (x, y) = self.pixel(nx, ny);
                    events.extend([EV_ABS, ABS_MT_SLOT, slot as i32]);
                    if held.is_none() {
                        let id = self.next_id;
                        self.next_id = if id >= 65535 { 1 } else { id + 1 };
                        self.down[slot] = Some(id);
                        events.extend([EV_ABS, ABS_MT_TRACKING_ID, id]);
                        events.extend([EV_ABS, ABS_MT_TOUCH_MAJOR, 8]);
                        events.extend([EV_ABS, ABS_MT_PRESSURE, 128]);
                    }
                    events.extend([EV_ABS, ABS_MT_POSITION_X, x]);
                    events.extend([EV_ABS, ABS_MT_POSITION_Y, y]);
                }
                (None, Some(_)) => {
                    self.down[slot] = None;
                    events.extend([EV_ABS, ABS_MT_SLOT, slot as i32]);
                    events.extend([EV_ABS, ABS_MT_TRACKING_ID, -1]);
                }
                (None, None) => {}
            }
        }
        if events.is_empty() {
            return None;
        }
        let now_down = self.any_down();
        if now_down != was_down {
            let v = i32::from(now_down);
            events.extend([EV_KEY, BTN_TOUCH, v, EV_KEY, BTN_TOOL_FINGER, v]);
        }
        events.extend([EV_SYN, SYN_REPORT, 0]);
        Some(inject(&events))
    }
}

/// How close to the screen's edge a scrolling finger may go.
const EDGE: f32 = 0.03;

/// Scrolling as a finger dragging the content (a phone has no wheel): down
/// at the first scroll, following each, lifted once they stop
/// ([`ScrollDrag::lift`]) after holding still, so the content doesn't fling.
#[derive(Debug, Default)]
pub struct ScrollDrag {
    at: Option<(f32, f32)>,
    /// Counts scrolls, so a lift planned after one knows whether another
    /// came since.
    pub count: u64,
}

/// A step of the scrolling finger, in fractions of the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Down(f32, f32),
    Move(f32, f32),
    Up,
}

impl ScrollDrag {
    /// The content moves by `dx`, `dy` (positive shows what is right or
    /// below, so the finger moves the other way) under the point `x`, `y`.
    pub fn scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> Vec<Step> {
        let inside = |(x, y): (f32, f32)| {
            (EDGE..=1.0 - EDGE).contains(&x) && (EDGE..=1.0 - EDGE).contains(&y)
        };
        let clamp = |v: f32| v.clamp(EDGE, 1.0 - EDGE);
        self.count += 1;
        let mut steps = Vec::new();
        let at = match self.at {
            Some(at) => at,
            None => {
                let start = (clamp(x), clamp(y));
                steps.push(Step::Down(start.0, start.1));
                start
            }
        };
        let mut to = (at.0 - dx, at.1 - dy);
        if !inside(to) && self.at.is_some() {
            // At the edge: lift and start again where the pointer is.
            steps.push(Step::Up);
            let start = (clamp(x), clamp(y));
            steps.push(Step::Down(start.0, start.1));
            to = (start.0 - dx, start.1 - dy);
        }
        let to = (clamp(to.0), clamp(to.1));
        steps.push(Step::Move(to.0, to.1));
        self.at = Some(to);
        steps
    }

    /// The scrolls stopped: lift the finger.
    pub fn lift(&mut self) -> bool {
        self.at.take().is_some()
    }
}
