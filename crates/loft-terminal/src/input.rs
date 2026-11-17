//! GPUI keystrokes → terminal key events.
//!
//! The view asks [`route_key_down`] what a key-down is for: text for the
//! platform's text system (so dead keys and input methods can compose), a
//! key event for libghostty's encoder, fixed bytes (macOS line editing), or
//! nothing (the app's own shortcuts). Text the platform commits comes back
//! through [`committed_text_input`]. The encoder itself, and the terminal
//! modes it follows, live in the backend.

use crate::backend::{KeyAction, KeyInput, Mods};
use gpui_kit::{Keystroke, Modifiers};

/// What to do with a key-down event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    /// Hand the key to the platform text system.
    Text,
    /// Encode and send this key event.
    Encode(KeyInput),
    /// Send these bytes verbatim.
    Bytes(&'static [u8]),
    /// Not for the terminal.
    Ignore,
}

/// Inputs that affect routing besides the keystroke itself.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouteContext {
    pub is_held: bool,
    pub caps_lock: bool,
    pub option_as_alt: bool,
    pub composing: bool,
}

/// ⌘← / ⌘→ / ⌘⌫ on macOS: what zsh and bash understand as line start, line
/// end and kill line.
const LINE_START: &[u8] = b"\x01";
const LINE_END: &[u8] = b"\x05";
const KILL_LINE: &[u8] = b"\x15";
/// ⌥← / ⌥→ on macOS: readline's word back / forward.
const WORD_BACK: &[u8] = b"\x1bb";
const WORD_FORWARD: &[u8] = b"\x1bf";

/// Whether `text` is something a key types: not empty, no control
/// characters, none of macOS's private-use function-key codepoints.
pub(crate) fn is_printable(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(&c))
}

pub(crate) fn route_key_down(keystroke: &Keystroke, ctx: RouteContext) -> KeyRoute {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();

    // An input method is composing: the key is its own.
    if ctx.composing {
        return KeyRoute::Ignore;
    }

    // Cmd chords are the app's, apart from macOS's line-editing keys.
    if m.platform {
        if cfg!(target_os = "macos") && !m.shift && !m.control && !m.alt {
            return match key {
                "left" => KeyRoute::Bytes(LINE_START),
                "right" => KeyRoute::Bytes(LINE_END),
                "backspace" => KeyRoute::Bytes(KILL_LINE),
                _ => KeyRoute::Ignore,
            };
        }
        return KeyRoute::Ignore;
    }

    // In a browser, Ctrl-Shift-V is the browser's paste: leave it be so its
    // paste event brings the text in.
    if cfg!(target_arch = "wasm32") && m.control && m.shift && key == "v" {
        return KeyRoute::Ignore;
    }

    // Word movement with Option, whatever Option-as-Alt says.
    if cfg!(target_os = "macos") && m.alt && !m.shift && !m.control {
        match key {
            "left" => return KeyRoute::Bytes(WORD_BACK),
            "right" => return KeyRoute::Bytes(WORD_FORWARD),
            _ => {}
        }
    }

    let typed = keystroke.key_char.as_deref().filter(|t| is_printable(t));
    if let Some(text) = typed
        && !m.control
        && !(m.alt && ctx.option_as_alt)
    {
        if !ctx.is_held {
            return KeyRoute::Text;
        }
        // A held key repeats at once, without the accent popup.
        return KeyRoute::Encode(KeyInput {
            key: keystroke.key.clone(),
            action: KeyAction::Repeat,
            mods: mods(m, ctx.caps_lock),
            consumed: text_consumed(m, ctx.caps_lock),
            text: Some(text.to_owned()),
            composing: false,
        });
    }

    KeyRoute::Encode(KeyInput {
        key: keystroke.key.clone(),
        action: if ctx.is_held {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        },
        mods: mods(m, ctx.caps_lock),
        consumed: Mods::default(),
        text: key_text(keystroke),
        composing: false,
    })
}

/// The text a non-text route carries: the key's own character (so the
/// encoder knows which letter a Ctrl or Meta chord is on), or the space a
/// Ctrl-Space types.
fn key_text(keystroke: &Keystroke) -> Option<String> {
    let m = &keystroke.modifiers;
    let mut chars = keystroke.key.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && !c.is_control()
    {
        let c = if m.shift && !m.control && c.is_ascii_lowercase() {
            c.to_ascii_uppercase()
        } else {
            c
        };
        return Some(c.to_string());
    }
    if keystroke.key == "space" {
        return keystroke
            .key_char
            .as_deref()
            .filter(|t| is_printable(t))
            .map(str::to_owned);
    }
    None
}

/// Modifiers the platform used up to produce typed text.
fn text_consumed(m: &Modifiers, caps_lock: bool) -> Mods {
    Mods {
        shift: m.shift,
        alt: m.alt,
        caps_lock,
        ..Mods::default()
    }
}

/// The key event for text the platform committed after a fresh key press.
pub(crate) fn committed_text_input(keystroke: &Keystroke, caps_lock: bool, text: &str) -> KeyInput {
    KeyInput {
        key: keystroke.key.clone(),
        action: KeyAction::Press,
        mods: mods(&keystroke.modifiers, caps_lock),
        consumed: text_consumed(&keystroke.modifiers, caps_lock),
        text: Some(text.to_owned()),
        composing: false,
    }
}

/// The key event for a key-up (bytes only under kitty's report-events).
pub(crate) fn key_up_input(keystroke: &Keystroke, caps_lock: bool) -> KeyInput {
    KeyInput {
        key: keystroke.key.clone(),
        action: KeyAction::Release,
        mods: mods(&keystroke.modifiers, caps_lock),
        consumed: Mods::default(),
        text: None,
        composing: false,
    }
}

pub(crate) fn mods(m: &Modifiers, caps_lock: bool) -> Mods {
    Mods {
        shift: m.shift,
        ctrl: m.control,
        alt: m.alt,
        super_key: m.platform,
        caps_lock,
    }
}

/// Byte offset of UTF-16 offset `utf16` in `text`; past the end maps to
/// the end.
pub(crate) fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units >= utf16 {
            return byte;
        }
        units += c.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TerminalBackend;
    use crate::backend::ghostty::GhosttyBackend;

    fn key(spec: &'static str, key_char: Option<&str>) -> Keystroke {
        let mut k = Keystroke::parse(spec).unwrap();
        k.key_char = key_char.map(str::to_owned);
        k
    }

    fn backend(option_as_alt: bool) -> GhosttyBackend {
        let mut b = GhosttyBackend::new(80, 24).unwrap();
        b.set_option_as_alt(option_as_alt);
        b
    }

    /// What the terminal ends up sending for a key-down, with the platform
    /// committing `key_char` for a text route.
    fn bytes_with(b: &mut GhosttyBackend, k: &Keystroke, ctx: RouteContext) -> Vec<u8> {
        match route_key_down(k, ctx) {
            KeyRoute::Text => {
                let text = k.key_char.clone().unwrap();
                let encoded = b.encode_key(&committed_text_input(k, ctx.caps_lock, &text));
                if encoded.is_empty() {
                    text.into_bytes()
                } else {
                    encoded
                }
            }
            KeyRoute::Encode(input) => b.encode_key(&input),
            KeyRoute::Bytes(bytes) => bytes.to_vec(),
            KeyRoute::Ignore => Vec::new(),
        }
    }

    fn bytes(k: &Keystroke, option_as_alt: bool) -> Vec<u8> {
        let ctx = RouteContext {
            option_as_alt,
            ..RouteContext::default()
        };
        bytes_with(&mut backend(option_as_alt), k, ctx)
    }

    #[test]
    fn plain_text_goes_through_the_platform_text_path() {
        let ctx = RouteContext::default();
        let a = key("a", Some("a"));
        let shift_a = key("shift-a", Some("A"));
        assert_eq!(route_key_down(&a, ctx), KeyRoute::Text);
        assert_eq!(route_key_down(&shift_a, ctx), KeyRoute::Text);
        assert_eq!(bytes(&a, false), b"a");
        assert_eq!(bytes(&shift_a, false), b"A");
        let option_s = key("alt-s", Some("ß"));
        assert_eq!(route_key_down(&option_s, ctx), KeyRoute::Text);
        assert_eq!(bytes(&option_s, false), "ß".as_bytes());
        assert_eq!(bytes(&key("space", Some(" ")), false), b" ");
    }

    #[test]
    fn named_keys_and_chords_encode_directly() {
        let cases: &[(&str, Option<&str>, &[u8])] = &[
            ("enter", Some("\n"), b"\r"),
            ("tab", Some("\t"), b"\t"),
            ("shift-tab", None, b"\x1b[Z"),
            ("backspace", None, b"\x7f"),
            ("escape", None, b"\x1b"),
            ("up", None, b"\x1b[A"),
            ("f3", None, b"\x1bOR"),
            ("f10", None, b"\x1b[21~"),
            ("ctrl-c", None, b"\x03"),
            ("ctrl-z", None, b"\x1a"),
            ("ctrl-space", Some(" "), b"\x00"),
            ("ctrl-\\", None, b"\x1c"),
            ("ctrl-right", None, b"\x1b[1;5C"),
        ];
        for (spec, key_char, expected) in cases {
            let k = key(spec, *key_char);
            assert!(
                matches!(
                    route_key_down(&k, RouteContext::default()),
                    KeyRoute::Encode(_)
                ),
                "{spec}"
            );
            assert_eq!(bytes(&k, false), *expected, "{spec}");
        }
        let caps = RouteContext {
            caps_lock: true,
            ..RouteContext::default()
        };
        assert_eq!(
            bytes_with(&mut backend(false), &key("ctrl-c", None), caps),
            b"\x03"
        );
    }

    #[test]
    fn option_as_alt_sends_an_escape_prefix() {
        let ctx = RouteContext {
            option_as_alt: true,
            ..RouteContext::default()
        };
        let option_b = key("alt-b", Some("∫"));
        assert!(matches!(
            route_key_down(&option_b, ctx),
            KeyRoute::Encode(_)
        ));
        assert_eq!(bytes(&option_b, true), b"\x1bb");
        assert_eq!(bytes(&key("alt-shift-b", Some("ı")), true), b"\x1bB");
        let option_left = bytes(&key("alt-left", None), true);
        if cfg!(target_os = "macos") {
            assert_eq!(option_left, b"\x1bb");
        } else {
            assert_eq!(option_left, b"\x1b[1;3D");
        }
        assert_eq!(bytes(&key("alt-shift-left", None), true), b"\x1b[1;4D");
    }

    #[test]
    fn key_repeat_skips_the_text_path() {
        let ctx = RouteContext {
            is_held: true,
            ..RouteContext::default()
        };
        let j = key("j", Some("j"));
        match route_key_down(&j, ctx) {
            KeyRoute::Encode(input) => assert_eq!(input.action, KeyAction::Repeat),
            other => panic!("{other:?}"),
        }
        assert_eq!(bytes_with(&mut backend(false), &j, ctx), b"j");
    }

    #[test]
    fn cmd_chords_belong_to_the_app() {
        let ctx = RouteContext::default();
        assert_eq!(route_key_down(&key("cmd-c", None), ctx), KeyRoute::Ignore);
        let left = route_key_down(&key("cmd-left", None), ctx);
        let backspace = route_key_down(&key("cmd-backspace", None), ctx);
        if cfg!(target_os = "macos") {
            assert_eq!(left, KeyRoute::Bytes(b"\x01"));
            assert_eq!(backspace, KeyRoute::Bytes(b"\x15"));
        } else {
            assert_eq!(left, KeyRoute::Ignore);
            assert_eq!(backspace, KeyRoute::Ignore);
        }
    }

    #[test]
    fn composition_owns_keys() {
        let ctx = RouteContext {
            composing: true,
            ..RouteContext::default()
        };
        assert_eq!(
            route_key_down(&key("backspace", None), ctx),
            KeyRoute::Ignore
        );
    }

    #[test]
    fn kitty_protocol_encodes_text_keys_with_their_key() {
        let a = key("a", Some("a"));
        let mut b = backend(false);
        b.write(b"\x1b[>8u");
        assert_eq!(
            b.encode_key(&committed_text_input(&a, false, "a")),
            b"\x1b[97u"
        );
        assert!(b.encode_key(&key_up_input(&a, false)).is_empty());
        b.write(b"\x1b[=10u");
        assert_eq!(b.encode_key(&key_up_input(&a, false)), b"\x1b[97;1:3u");
    }

    #[test]
    fn utf16_offsets_map_to_bytes() {
        let s = "aé😀b";
        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 1), 1);
        assert_eq!(utf16_to_byte(s, 2), 3);
        assert_eq!(utf16_to_byte(s, 4), 7);
        assert_eq!(utf16_to_byte(s, 9), s.len());
    }

    #[test]
    fn private_use_and_controls_are_not_text() {
        assert!(is_printable("a"));
        assert!(is_printable("ß"));
        assert!(!is_printable(""));
        assert!(!is_printable("\n"));
        assert!(!is_printable("\u{F700}"));
    }
}
