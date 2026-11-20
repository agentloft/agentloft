//! One modifier key on its own (right ⌥, right ⌘, Fn, …) as the dictation
//! key. GPUI's key bindings can't name a lone modifier, and its modifier
//! events don't tell left from right, so the platform's own key events are
//! watched: an AppKit local event monitor on a Mac (key codes say which
//! side), `KeyboardEvent.code` in a browser. Elsewhere nothing is reported
//! ([`supported`] is false) and only key combinations can be used.

use futures::channel::mpsc;
use std::cell::RefCell;

/// A modifier key, by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideKey {
    RightOption,
    LeftOption,
    RightCommand,
    LeftCommand,
    RightControl,
    LeftControl,
    RightShift,
    LeftShift,
    Fn,
}

impl SideKey {
    pub const ALL: [SideKey; 9] = [
        SideKey::RightOption,
        SideKey::LeftOption,
        SideKey::RightCommand,
        SideKey::LeftCommand,
        SideKey::RightControl,
        SideKey::LeftControl,
        SideKey::RightShift,
        SideKey::LeftShift,
        SideKey::Fn,
    ];

    /// Its name in `[voice] key`.
    pub fn id(self) -> &'static str {
        match self {
            SideKey::RightOption => "right_option",
            SideKey::LeftOption => "left_option",
            SideKey::RightCommand => "right_command",
            SideKey::LeftCommand => "left_command",
            SideKey::RightControl => "right_control",
            SideKey::LeftControl => "left_control",
            SideKey::RightShift => "right_shift",
            SideKey::LeftShift => "left_shift",
            SideKey::Fn => "fn",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id.trim())
    }

    /// For people: "Right ⌥" on a Mac, "Right Alt" elsewhere.
    pub fn label(self) -> &'static str {
        let mac = cfg!(target_os = "macos") || crate::host::IS_WEB;
        match (self, mac) {
            (SideKey::RightOption, true) => "Right ⌥",
            (SideKey::LeftOption, true) => "Left ⌥",
            (SideKey::RightCommand, true) => "Right ⌘",
            (SideKey::LeftCommand, true) => "Left ⌘",
            (SideKey::RightControl, true) => "Right ⌃",
            (SideKey::LeftControl, true) => "Left ⌃",
            (SideKey::RightShift, true) => "Right ⇧",
            (SideKey::LeftShift, true) => "Left ⇧",
            (SideKey::RightOption, false) => "Right Alt",
            (SideKey::LeftOption, false) => "Left Alt",
            (SideKey::RightCommand, false) => "Right Super",
            (SideKey::LeftCommand, false) => "Left Super",
            (SideKey::RightControl, false) => "Right Ctrl",
            (SideKey::LeftControl, false) => "Left Ctrl",
            (SideKey::RightShift, false) => "Right Shift",
            (SideKey::LeftShift, false) => "Left Shift",
            (SideKey::Fn, _) => "Fn",
        }
    }
}

// Nothing reports these on Windows and Linux yet.
#[cfg_attr(
    not(any(target_os = "macos", target_arch = "wasm32")),
    allow(dead_code)
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Down(SideKey),
    Up(SideKey),
    /// Any other key went down (so a held modifier was part of a
    /// combination, like ⌥E).
    Other,
}

/// Whether lone modifier keys can be watched here.
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_arch = "wasm32"))
}

thread_local! {
    static LISTENERS: RefCell<Vec<mpsc::UnboundedSender<KeyEvent>>> = const { RefCell::new(Vec::new()) };
    static WATCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Key events from now on, for as long as the receiver is kept (call on
/// the main thread).
pub fn subscribe() -> mpsc::UnboundedReceiver<KeyEvent> {
    let (tx, rx) = mpsc::unbounded();
    LISTENERS.with(|l| l.borrow_mut().push(tx));
    if !WATCHING.get() {
        WATCHING.set(true);
        platform::watch();
    }
    rx
}

#[cfg_attr(
    not(any(target_os = "macos", target_arch = "wasm32")),
    allow(dead_code)
)]
fn send(event: KeyEvent) {
    LISTENERS.with(|l| {
        l.borrow_mut().retain(|tx| tx.unbounded_send(event).is_ok());
    });
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // objc2 calls into AppKit; each block has a SAFETY note.
mod platform {
    use super::{KeyEvent, SideKey, send};
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
    use std::ptr::NonNull;

    /// Device-dependent modifier bits (IOKit's `NX_DEVICE*KEYMASK`): which
    /// side is down, which the device-independent flags don't say.
    fn held(key: SideKey, flags: usize) -> bool {
        let bit = match key {
            SideKey::LeftControl => 0x0001,
            SideKey::LeftShift => 0x0002,
            SideKey::RightShift => 0x0004,
            SideKey::LeftCommand => 0x0008,
            SideKey::RightCommand => 0x0010,
            SideKey::LeftOption => 0x0020,
            SideKey::RightOption => 0x0040,
            SideKey::RightControl => 0x2000,
            SideKey::Fn => 1 << 23,
        };
        flags & bit != 0
    }

    /// The key a `flagsChanged` event's key code is (`kVK_*`).
    fn key_of(code: u16) -> Option<SideKey> {
        Some(match code {
            54 => SideKey::RightCommand,
            55 => SideKey::LeftCommand,
            56 => SideKey::LeftShift,
            58 => SideKey::LeftOption,
            59 => SideKey::LeftControl,
            60 => SideKey::RightShift,
            61 => SideKey::RightOption,
            62 => SideKey::RightControl,
            63 => SideKey::Fn,
            _ => return None,
        })
    }

    /// A dev hook's press or release of `key`: a real `flagsChanged` event,
    /// queued like one from the keyboard (through the monitor and all).
    pub(super) fn post(key: SideKey, down: bool) {
        use objc2_app_kit::{NSApplication, NSEventModifierFlags};
        use objc2_foundation::{MainThreadMarker, NSPoint, NSString};
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let code = (54..=63u16).find(|c| key_of(*c) == Some(key)).unwrap_or(0);
        let device = (0..24)
            .map(|b| 1usize << b)
            .find(|bit| held(key, *bit))
            .unwrap_or(0);
        let general = match key {
            SideKey::LeftShift | SideKey::RightShift => 1 << 17,
            SideKey::LeftControl | SideKey::RightControl => 1 << 18,
            SideKey::LeftOption | SideKey::RightOption => 1 << 19,
            SideKey::LeftCommand | SideKey::RightCommand => 1 << 20,
            SideKey::Fn => 1 << 23,
        };
        let flags = if down {
            general | device | 0x100
        } else {
            0x100
        };
        let empty = NSString::from_str("");
        let Some(event) = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::FlagsChanged,
            NSPoint::new(0., 0.),
            NSEventModifierFlags(flags),
            0.,
            0,
            None,
            &empty,
            &empty,
            false,
            code,
        ) else {
            return;
        };
        NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
    }

    pub(super) fn watch() {
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit passes a live event for the length of the call.
            let e = unsafe { event.as_ref() };
            let kind = e.r#type();
            if kind == NSEventType::FlagsChanged {
                if let Some(key) = key_of(e.keyCode()) {
                    let down = held(key, e.modifierFlags().0);
                    send(if down {
                        KeyEvent::Down(key)
                    } else {
                        KeyEvent::Up(key)
                    });
                }
            } else if kind == NSEventType::KeyDown {
                send(KeyEvent::Other);
            }
            // Watched, not taken: the event goes on as it would have.
            event.as_ptr()
        });
        // SAFETY: the block returns the event it was given; AppKit keeps the
        // monitor for the life of the app, which is what is wanted.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::FlagsChanged | NSEventMask::KeyDown,
                &block,
            )
        };
        std::mem::forget(monitor);
    }
}

#[cfg(target_arch = "wasm32")]
mod platform {
    use super::{KeyEvent, SideKey, send};
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;

    fn key_of(code: &str) -> Option<SideKey> {
        Some(match code {
            "AltRight" => SideKey::RightOption,
            "AltLeft" => SideKey::LeftOption,
            "MetaRight" | "OSRight" => SideKey::RightCommand,
            "MetaLeft" | "OSLeft" => SideKey::LeftCommand,
            "ControlRight" => SideKey::RightControl,
            "ControlLeft" => SideKey::LeftControl,
            "ShiftRight" => SideKey::RightShift,
            "ShiftLeft" => SideKey::LeftShift,
            "Fn" => SideKey::Fn,
            _ => return None,
        })
    }

    pub(super) fn watch() {
        let Some(window) = web_sys::window() else {
            return;
        };
        let down =
            Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(
                |e: web_sys::KeyboardEvent| match key_of(&e.code()) {
                    Some(_) if e.repeat() => {}
                    Some(key) => send(KeyEvent::Down(key)),
                    None => send(KeyEvent::Other),
                },
            );
        let up = Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
            if let Some(key) = key_of(&e.code()) {
                send(KeyEvent::Up(key));
            }
        });
        // Capture phase: seen before anything on the page can stop them.
        let _ = window.add_event_listener_with_callback_and_bool(
            "keydown",
            down.as_ref().unchecked_ref(),
            true,
        );
        let _ = window.add_event_listener_with_callback_and_bool(
            "keyup",
            up.as_ref().unchecked_ref(),
            true,
        );
        // For the life of the page.
        down.forget();
        up.forget();
    }
}

#[cfg(not(any(target_os = "macos", target_arch = "wasm32")))]
mod platform {
    pub(super) fn watch() {}
}

/// `AGENTLOFT_DEV_TAB=sidekey:<id>:down|up`: press or let go of a lone
/// modifier as the keyboard would (a Mac only).
pub fn dev_press(key: SideKey, down: bool) {
    #[cfg(target_os = "macos")]
    platform::post(key, down);
    #[cfg(not(target_os = "macos"))]
    let _ = (key, down);
}

#[cfg(test)]
mod tests {
    use super::SideKey;

    #[test]
    fn keys_round_trip_through_their_ids() {
        for key in SideKey::ALL {
            assert_eq!(SideKey::parse(key.id()), Some(key));
        }
        assert_eq!(SideKey::parse(" right_option "), Some(SideKey::RightOption));
        assert_eq!(SideKey::parse("cmd-shift-d"), None);
    }
}
