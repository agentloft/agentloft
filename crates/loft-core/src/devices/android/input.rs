//! Turning the panel's input (fractions of the picture, DOM key names) into
//! what the emulator takes (pixels of its display as built, W3C key names).

use loft_protocol::{DeviceButton, page_modifiers};

/// How the device is turned, as the emulator reports it with each picture
/// (`Rotation.SkinRotation`): quarter turns counter-clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Turn {
    #[default]
    Portrait,
    /// Turned a quarter counter-clockwise: the device's top is on the left.
    Left,
    UpsideDown,
    /// Turned a quarter clockwise: the device's top is on the right.
    Right,
}

impl Turn {
    pub fn from_skin(rotation: i32) -> Self {
        match rotation.rem_euclid(4) {
            1 => Self::Left,
            2 => Self::UpsideDown,
            3 => Self::Right,
            _ => Self::Portrait,
        }
    }

    /// The nearest quarter turn to `z` degrees of the rotation sensor.
    pub fn from_degrees(z: f32) -> Self {
        Self::from_skin((z / 90.0).round() as i32)
    }

    pub fn quarters(self) -> i32 {
        match self {
            Self::Portrait => 0,
            Self::Left => 1,
            Self::UpsideDown => 2,
            Self::Right => 3,
        }
    }

    pub fn sideways(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    /// The screen's size as the pictures show it, from its size as built.
    pub fn size(self, width: u32, height: u32) -> (u32, u32) {
        if self.sideways() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// A point of the picture (fractions, as the user sees the screen) as a
    /// point of the display as built, in pixels: the touch screen doesn't
    /// turn with the picture.
    pub fn to_display(self, x: f32, y: f32, width: u32, height: u32) -> (i32, i32) {
        let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let (nx, ny) = match self {
            Self::Portrait => (x, y),
            // The device's top-right corner is at the picture's top left.
            Self::Left => (1.0 - y, x),
            Self::UpsideDown => (1.0 - x, 1.0 - y),
            Self::Right => (y, 1.0 - x),
        };
        let px =
            |f: f32, n: u32| ((f * n as f32).round() as i32).clamp(0, n.saturating_sub(1) as i32);
        (px(nx, width), px(ny, height))
    }

    /// The emulator's rotation sensor (degrees about z) for this turn.
    pub fn degrees(self) -> f32 {
        self.quarters() as f32 * 90.0
    }

    pub fn turned(self, counter_clockwise: bool) -> Self {
        Self::from_skin(self.quarters() + if counter_clockwise { 1 } else { -1 })
    }
}

/// The W3C key name the emulator takes for a device button; `None` for
/// turning, which isn't a key.
pub fn button_key(button: DeviceButton) -> Option<&'static str> {
    Some(match button {
        DeviceButton::Home => "GoHome",
        DeviceButton::Back => "GoBack",
        DeviceButton::Recents => "AppSwitch",
        DeviceButton::Power => "Power",
        DeviceButton::VolumeUp => "AudioVolumeUp",
        DeviceButton::VolumeDown => "AudioVolumeDown",
        DeviceButton::RotateLeft | DeviceButton::RotateRight => return None,
    })
}

/// A DOM `KeyboardEvent.key` as the emulator's W3C name, or `None` for keys
/// an Android device has no use for. One printable character passes as is.
pub fn key_name(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return (!c.is_control()).then(|| key.to_owned());
    }
    let named = match key {
        "Esc" => "Escape",
        "Return" => "Enter",
        "Del" => "Delete",
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        "Up" => "ArrowUp",
        "Down" => "ArrowDown",
        "Space" | "Spacebar" => " ",
        "Win" | "OS" | "Command" | "Super" => "Meta",
        "Enter" | "Backspace" | "Tab" | "Escape" | "Delete" | "Insert" | "ArrowLeft"
        | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End" | "PageUp" | "PageDown"
        | "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" | "ContextMenu" | "F1" | "F2"
        | "F3" | "F4" | "F5" | "F6" | "F7" | "F8" | "F9" | "F10" | "F11" | "F12" | "GoBack"
        | "GoHome" | "AppSwitch" | "Power" | "AudioVolumeUp" | "AudioVolumeDown"
        | "AudioVolumeMute" | "MediaPlayPause" | "MediaTrackNext" | "MediaTrackPrevious"
        | "MediaStop" => key,
        _ => return None,
    };
    Some(named.to_owned())
}

/// Android's `KeyEvent` code for a key named as [`key_name`] names it, for
/// sending through adb (`input keyevent`) when the device has no keyboard
/// for the emulator to type on. Letters and digits too; other characters
/// are typed as text.
pub fn android_keycode(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c.to_ascii_lowercase() {
            c @ 'a'..='z' => Some(29 + (c as u32 - 'a' as u32)),
            c @ '0'..='9' => Some(7 + (c as u32 - '0' as u32)),
            ' ' => Some(62),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
        && (1..=12).contains(&n)
    {
        return Some(130 + n);
    }
    Some(match name {
        "Enter" => 66,
        "Backspace" => 67,
        "Delete" => 112,
        "Tab" => 61,
        "Escape" => 111,
        "ArrowLeft" => 21,
        "ArrowRight" => 22,
        "ArrowUp" => 19,
        "ArrowDown" => 20,
        "Home" => 122,
        "End" => 123,
        "PageUp" => 92,
        "PageDown" => 93,
        "Insert" => 124,
        "Shift" => 59,
        "Control" => 113,
        "Alt" => 57,
        "Meta" => 117,
        "CapsLock" => 115,
        "ContextMenu" => 82,
        "GoBack" => 4,
        "GoHome" => 3,
        "AppSwitch" => 187,
        "Power" => 26,
        "AudioVolumeUp" => 24,
        "AudioVolumeDown" => 25,
        "AudioVolumeMute" => 164,
        "MediaPlayPause" => 85,
        "MediaStop" => 86,
        "MediaTrackNext" => 87,
        "MediaTrackPrevious" => 88,
        _ => return None,
    })
}

/// Android's `KEYCODE_PASTE`.
pub const PASTE_KEYCODE: u32 = 279;

/// The modifier keys held for a key, in the order they go down.
pub fn modifier_keys(modifiers: u8) -> Vec<&'static str> {
    [
        (page_modifiers::CONTROL, "Control"),
        (page_modifiers::ALT, "Alt"),
        (page_modifiers::SHIFT, "Shift"),
        (page_modifiers::META, "Meta"),
    ]
    .into_iter()
    .filter(|(bit, _)| modifiers & bit != 0)
    .map(|(_, name)| name)
    .collect()
}

/// Text the emulator can type key by key (it knows printable ASCII);
/// anything else goes through the clipboard.
pub fn typeable(text: &str) -> bool {
    text.chars()
        .all(|c| c == '\n' || c == '\t' || (' '..='~').contains(&c))
}

/// How close to the screen's edge a scrolling finger may go.
const EDGE: f32 = 0.03;

/// A step of the finger that scrolls, in fractions of the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Finger {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
}

/// Scrolling as a finger dragging the content: Android's own way of
/// scrolling everything, where a wheel needs a mouse the device may not
/// have. The finger goes down at the first scroll, follows each one, and
/// is lifted once the scrolls stop ([`ScrollDrag::lift`]), after holding
/// still, so the content doesn't fling on.
#[derive(Debug, Default)]
pub struct ScrollDrag {
    at: Option<(f32, f32)>,
    /// Counts scrolls, so a lift planned after one knows whether another
    /// came since.
    pub count: u64,
}

impl ScrollDrag {
    /// The content moves by `dx`, `dy` (positive shows what is right or
    /// below, so the finger moves the other way) under the point `x`, `y`.
    pub fn scroll(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> Vec<Finger> {
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
                steps.push(Finger::Down(start.0, start.1));
                start
            }
        };
        let mut to = (at.0 - dx, at.1 - dy);
        if !inside(to) && self.at.is_some() {
            // At the edge: lift and start again where the pointer is.
            steps.push(Finger::Up(at.0, at.1));
            let start = (clamp(x), clamp(y));
            steps.push(Finger::Down(start.0, start.1));
            to = (start.0 - dx, start.1 - dy);
        }
        let to = (clamp(to.0), clamp(to.1));
        steps.push(Finger::Move(to.0, to.1));
        self.at = Some(to);
        steps
    }

    /// The scrolls stopped: lift the finger where it is.
    pub fn lift(&mut self) -> Option<Finger> {
        self.at.take().map(|(x, y)| Finger::Up(x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_follow_the_turn() {
        let (w, h) = (1080, 2400);
        assert_eq!(Turn::Portrait.to_display(0.5, 0.25, w, h), (540, 600));
        // Clamped onto the screen.
        assert_eq!(Turn::Portrait.to_display(1.2, -0.1, w, h), (1079, 0));
        // Turned left, the picture's top left is the display's top right.
        assert_eq!(Turn::Left.to_display(0.0, 0.0, w, h), (1079, 0));
        assert_eq!(Turn::Left.to_display(0.0, 1.0, w, h), (0, 0));
        assert_eq!(Turn::Left.to_display(1.0, 1.0, w, h), (0, 2399));
        assert_eq!(Turn::Right.to_display(0.0, 0.0, w, h), (0, 2399));
        assert_eq!(Turn::Right.to_display(1.0, 0.0, w, h), (0, 0));
        assert_eq!(Turn::UpsideDown.to_display(0.0, 0.0, w, h), (1079, 2399));
        assert_eq!(Turn::Left.to_display(0.25, 0.5, w, h), (540, 600));
    }

    #[test]
    fn turning_goes_round() {
        assert_eq!(Turn::Portrait.turned(true), Turn::Left);
        assert_eq!(Turn::Left.turned(true), Turn::UpsideDown);
        assert_eq!(Turn::Portrait.turned(false), Turn::Right);
        assert_eq!(Turn::Right.turned(true), Turn::Portrait);
        assert_eq!(Turn::from_skin(-1), Turn::Right);
        assert_eq!(Turn::Left.size(1080, 2400), (2400, 1080));
        assert_eq!(Turn::UpsideDown.size(1080, 2400), (1080, 2400));
        assert_eq!(Turn::Right.degrees(), 270.0);
        assert_eq!(Turn::from_degrees(-90.0), Turn::Right);
        assert_eq!(Turn::from_degrees(88.0), Turn::Left);
        assert_eq!(Turn::from_degrees(180.0), Turn::UpsideDown);
        assert_eq!(Turn::from_degrees(0.4), Turn::Portrait);
    }

    #[test]
    fn keys_map_to_the_emulators_names() {
        assert_eq!(key_name("Enter").as_deref(), Some("Enter"));
        assert_eq!(key_name("Esc").as_deref(), Some("Escape"));
        assert_eq!(key_name("Left").as_deref(), Some("ArrowLeft"));
        assert_eq!(key_name("a").as_deref(), Some("a"));
        assert_eq!(key_name("é").as_deref(), Some("é"));
        assert_eq!(key_name("Spacebar").as_deref(), Some(" "));
        assert_eq!(key_name("\u{8}"), None);
        assert_eq!(key_name("Hyper"), None);
        assert_eq!(key_name(""), None);
        assert_eq!(button_key(DeviceButton::Back), Some("GoBack"));
        assert_eq!(button_key(DeviceButton::Recents), Some("AppSwitch"));
        assert_eq!(button_key(DeviceButton::RotateLeft), None);
        assert_eq!(
            modifier_keys(page_modifiers::SHIFT | page_modifiers::CONTROL),
            vec!["Control", "Shift"]
        );
        assert!(modifier_keys(0).is_empty());
    }

    #[test]
    fn keys_have_android_codes() {
        assert_eq!(android_keycode("GoHome"), Some(3));
        assert_eq!(android_keycode("GoBack"), Some(4));
        assert_eq!(android_keycode("Enter"), Some(66));
        assert_eq!(android_keycode("a"), Some(29));
        assert_eq!(android_keycode("Z"), Some(54));
        assert_eq!(android_keycode("0"), Some(7));
        assert_eq!(android_keycode("F1"), Some(131));
        assert_eq!(android_keycode("F12"), Some(142));
        assert_eq!(android_keycode("F13"), None);
        assert_eq!(android_keycode("!"), None);
        // Every key and button the emulator takes by name has one too.
        for button in [
            DeviceButton::Home,
            DeviceButton::Back,
            DeviceButton::Recents,
            DeviceButton::Power,
            DeviceButton::VolumeUp,
            DeviceButton::VolumeDown,
        ] {
            assert!(android_keycode(button_key(button).unwrap()).is_some());
        }
        for key in [
            "Esc",
            "Left",
            "Del",
            "Spacebar",
            "Meta",
            "PageDown",
            "MediaStop",
        ] {
            assert!(android_keycode(&key_name(key).unwrap()).is_some(), "{key}");
        }
    }

    #[test]
    fn text_outside_ascii_is_not_typed() {
        assert!(typeable("hello, world!\n"));
        assert!(!typeable("café"));
        assert!(!typeable("👋"));
    }

    #[test]
    fn scrolls_drag_a_finger_the_other_way() {
        let mut drag = ScrollDrag::default();
        assert_eq!(
            drag.scroll(0.5, 0.6, 0.0, 0.1),
            vec![Finger::Down(0.5, 0.6), Finger::Move(0.5, 0.5)]
        );
        // The next one carries on from there, wherever the pointer is.
        assert_eq!(
            drag.scroll(0.5, 0.6, 0.05, 0.1),
            vec![Finger::Move(0.45, 0.4)]
        );
        // Past the edge: lifted, and down again under the pointer.
        let steps = drag.scroll(0.5, 0.6, 0.0, 0.5);
        assert_eq!(steps.len(), 3);
        assert!(matches!(steps[0], Finger::Up(..)));
        assert_eq!(steps[1], Finger::Down(0.5, 0.6));
        assert!(matches!(steps[2], Finger::Move(x, y) if x == 0.5 && (y - 0.1).abs() < 1e-6));
        assert_eq!(drag.count, 3);
        assert!(matches!(drag.lift(), Some(Finger::Up(..))));
        assert_eq!(drag.lift(), None);
        // A huge one from the edge stays on the screen.
        let steps = drag.scroll(0.0, 1.0, 0.0, 3.0);
        assert_eq!(steps[0], Finger::Down(EDGE, 1.0 - EDGE));
        assert_eq!(steps[1], Finger::Move(EDGE, EDGE));
    }
}
