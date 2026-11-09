//! GPUI keystrokes → terminal key events.
//!
//! The split mirrors how a native macOS terminal treats the keyboard:
//!
//! * A plain text key's first press goes to the platform text system
//!   (`insertText` → [`EntityInputHandler::replace_text_in_range`]), so dead
//!   keys and IMEs can compose. The view remembers the key and encodes the
//!   committed text together with it, which keeps the kitty keyboard
//!   protocol exact.
//! * Everything else (named keys, Ctrl/Alt chords, key repeats) is encoded
//!   right away by the engine's key encoder.
//! * Cmd chords belong to the app, except Ghostty's macOS line-editing
//!   defaults (Cmd-Left/Right/Backspace).
//!
//! Keystroke-mapping rules adapted from Ghostex's `terminal_element.rs`
//! (MIT, see THIRD_PARTY_NOTICES.md).
//!
//! [`EntityInputHandler::replace_text_in_range`]: gpui_kit::EntityInputHandler::replace_text_in_range

use crate::backend::{KeyAction, KeyInput, Mods};
use gpui_kit::{Keystroke, Modifiers};

/// What to do with a key-down event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    /// Let the platform text input path handle it; the committed text is
    /// encoded together with this key.
    Text,
    /// Encode and send this key event.
    Encode(KeyInput),
    /// Send these bytes verbatim.
    Bytes(&'static [u8]),
    /// Not for the terminal; let app key bindings have it.
    Ignore,
}

/// Inputs that affect routing besides the keystroke itself.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouteContext {
    pub is_held: bool,
    pub caps_lock: bool,
    /// Treat Option as Alt (Meta) instead of a character modifier.
    pub option_as_alt: bool,
    /// An IME composition is in progress.
    pub composing: bool,
}

pub(crate) fn route_key_down(keystroke: &Keystroke, ctx: RouteContext) -> KeyRoute {
    let m = &keystroke.modifiers;
    if ctx.composing {
        return KeyRoute::Ignore;
    }
    if m.platform {
        return match line_editing_bytes(keystroke) {
            Some(bytes) => KeyRoute::Bytes(bytes),
            None => KeyRoute::Ignore,
        };
    }
    if let Some(bytes) = word_navigation_bytes(keystroke) {
        return KeyRoute::Bytes(bytes);
    }
    let text = printable(keystroke.key_char.as_deref());
    let alt_as_meta = m.alt && ctx.option_as_alt;
    if let Some(text) = text
        && !m.control
        && !alt_as_meta
    {
        if !ctx.is_held {
            return KeyRoute::Text;
        }
        // Repeats skip the platform text path (no press-and-hold popup).
        return KeyRoute::Encode(KeyInput {
            key: keystroke.key.clone(),
            action: KeyAction::Repeat,
            mods: mods(m, ctx.caps_lock),
            consumed: consumed_by_text(m, ctx.caps_lock),
            text: Some(text.to_owned()),
            composing: false,
        });
    }

    // Ctrl/Alt chords and named keys: give the encoder the unmodified
    // character of the physical key, and leave every modifier unconsumed so
    // the encoder applies them itself.
    let text = single_char_key(keystroke)
        .or_else(|| text.filter(|_| keystroke.key == "space").map(str::to_owned));
    KeyRoute::Encode(KeyInput {
        key: keystroke.key.clone(),
        action: if ctx.is_held {
            KeyAction::Repeat
        } else {
            KeyAction::Press
        },
        mods: mods(m, ctx.caps_lock),
        consumed: Mods::default(),
        text,
        composing: false,
    })
}

/// The key event for text the platform committed for `keystroke`.
pub(crate) fn committed_text_input(keystroke: &Keystroke, caps_lock: bool, text: &str) -> KeyInput {
    KeyInput {
        key: keystroke.key.clone(),
        action: KeyAction::Press,
        mods: mods(&keystroke.modifiers, caps_lock),
        consumed: consumed_by_text(&keystroke.modifiers, caps_lock),
        text: Some(text.to_owned()),
        composing: false,
    }
}

/// The key-up event, relevant only under kitty's report-events mode.
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

/// Modifiers the platform already folded into produced text (Shift in "A",
/// Option in "ß", Caps Lock in "A"). Ctrl and Cmd never are.
fn consumed_by_text(m: &Modifiers, caps_lock: bool) -> Mods {
    Mods {
        shift: m.shift,
        alt: m.alt,
        caps_lock,
        ..Mods::default()
    }
}

/// The physical key's own character for chords (`ctrl-c` → "c"), with an
/// ASCII letter upper-cased when Shift is held.
fn single_char_key(keystroke: &Keystroke) -> Option<String> {
    let mut chars = keystroke.key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if c.is_control() {
        return None;
    }
    let c = if keystroke.modifiers.shift && c.is_ascii_lowercase() && !keystroke.modifiers.control {
        c.to_ascii_uppercase()
    } else {
        c
    };
    Some(c.to_string())
}

/// Text usable as typed input: no controls (Enter/Tab arrive as "\n"/"\t"),
/// no macOS function-key private-use codepoints, not empty (dead keys).
fn printable(text: Option<&str>) -> Option<&str> {
    let text = text?;
    (!text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c)))
    .then_some(text)
}

/// Ghostty's macOS default keybinds for shell line editing: Cmd-Left/Right
/// jump to line start/end (Ctrl-A / Ctrl-E), Cmd-Backspace kills the line
/// (Ctrl-U).
fn line_editing_bytes(keystroke: &Keystroke) -> Option<&'static [u8]> {
    let m = &keystroke.modifiers;
    if !cfg!(target_os = "macos") || m.shift || m.control || m.alt {
        return None;
    }
    match keystroke.key.as_str() {
        "left" => Some(b"\x01"),
        "right" => Some(b"\x05"),
        "backspace" => Some(b"\x15"),
        _ => None,
    }
}

/// Ghostty's macOS default keybinds for word movement: Option-Left/Right
/// send `ESC b` / `ESC f` (readline backward/forward-word), whatever the
/// Option-as-Alt setting.
fn word_navigation_bytes(keystroke: &Keystroke) -> Option<&'static [u8]> {
    let m = &keystroke.modifiers;
    if !cfg!(target_os = "macos") || !m.alt || m.shift || m.control || m.platform {
        return None;
    }
    match keystroke.key.as_str() {
        "left" => Some(b"\x1bb"),
        "right" => Some(b"\x1bf"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::TerminalBackend as _;
    use crate::backend::ghostty::GhosttyBackend;

    fn ks(key: &str, key_char: Option<&str>, f: impl FnOnce(&mut Modifiers)) -> Keystroke {
        let mut modifiers = Modifiers::default();
        f(&mut modifiers);
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn ctx() -> RouteContext {
        RouteContext::default()
    }

    /// Route through the real encoder, following a `Text` route the way the
    /// platform would (committing `key_char`).
    fn send(keystroke: &Keystroke, ctx: RouteContext) -> Vec<u8> {
        let mut backend = GhosttyBackend::new(80, 24).unwrap();
        backend.set_option_as_alt(ctx.option_as_alt);
        match route_key_down(keystroke, ctx) {
            KeyRoute::Text => {
                let text = keystroke.key_char.as_deref().unwrap();
                backend.encode_key(&committed_text_input(keystroke, ctx.caps_lock, text))
            }
            KeyRoute::Encode(input) => backend.encode_key(&input),
            KeyRoute::Bytes(bytes) => bytes.to_vec(),
            KeyRoute::Ignore => Vec::new(),
        }
    }

    #[test]
    fn plain_text_goes_through_the_platform_text_path() {
        assert_eq!(
            route_key_down(&ks("a", Some("a"), |_| {}), ctx()),
            KeyRoute::Text
        );
        let shift_a = ks("a", Some("A"), |m| m.shift = true);
        assert_eq!(route_key_down(&shift_a, ctx()), KeyRoute::Text);
        assert_eq!(send(&shift_a, ctx()), b"A");
        // Option composes characters unless it acts as Alt.
        let opt_s = ks("s", Some("ß"), |m| m.alt = true);
        assert_eq!(route_key_down(&opt_s, ctx()), KeyRoute::Text);
        assert_eq!(send(&opt_s, ctx()), "ß".as_bytes());
        assert_eq!(send(&ks("space", Some(" "), |_| {}), ctx()), b" ");
    }

    #[test]
    fn named_keys_and_chords_encode_directly() {
        assert_eq!(send(&ks("enter", Some("\n"), |_| {}), ctx()), b"\r");
        assert_eq!(send(&ks("tab", Some("\t"), |_| {}), ctx()), b"\t");
        assert_eq!(
            send(&ks("tab", Some("\t"), |m| m.shift = true), ctx()),
            b"\x1b[Z"
        );
        assert_eq!(send(&ks("backspace", None, |_| {}), ctx()), b"\x7f");
        assert_eq!(send(&ks("escape", None, |_| {}), ctx()), b"\x1b");
        assert_eq!(send(&ks("up", None, |_| {}), ctx()), b"\x1b[A");
        assert_eq!(send(&ks("f3", None, |_| {}), ctx()), b"\x1bOR");
        assert_eq!(send(&ks("f10", None, |_| {}), ctx()), b"\x1b[21~");
        // GPUI reports ctrl chords without key_char.
        assert_eq!(send(&ks("c", None, |m| m.control = true), ctx()), b"\x03");
        assert_eq!(send(&ks("z", None, |m| m.control = true), ctx()), b"\x1a");
        assert_eq!(
            send(&ks("space", Some(" "), |m| m.control = true), ctx()),
            b"\x00"
        );
        assert_eq!(send(&ks("\\", None, |m| m.control = true), ctx()), b"\x1c");
        assert_eq!(
            send(&ks("right", None, |m| m.control = true), ctx()),
            b"\x1b[1;5C"
        );
        // Caps Lock does not break Ctrl-C.
        let caps = RouteContext {
            caps_lock: true,
            ..ctx()
        };
        assert_eq!(send(&ks("c", None, |m| m.control = true), caps), b"\x03");
    }

    #[test]
    fn option_as_alt_sends_escape_prefix() {
        let meta = RouteContext {
            option_as_alt: true,
            ..ctx()
        };
        let opt_b = ks("b", Some("∫"), |m| m.alt = true);
        assert!(matches!(route_key_down(&opt_b, meta), KeyRoute::Encode(_)));
        assert_eq!(send(&opt_b, meta), b"\x1bb");
        let opt_shift_b = ks("b", Some("ı"), |m| {
            m.alt = true;
            m.shift = true;
        });
        assert_eq!(send(&opt_shift_b, meta), b"\x1bB");
        let word_left: &[u8] = if cfg!(target_os = "macos") {
            b"\x1bb" // Ghostty's macOS Option-Left keybind
        } else {
            b"\x1b[1;3D"
        };
        assert_eq!(send(&ks("left", None, |m| m.alt = true), meta), word_left);
        let shifted = ks("left", None, |m| {
            m.alt = true;
            m.shift = true;
        });
        assert_eq!(send(&shifted, meta), b"\x1b[1;4D");
    }

    #[test]
    fn key_repeat_skips_the_text_path() {
        let held = RouteContext {
            is_held: true,
            ..ctx()
        };
        let route = route_key_down(&ks("j", Some("j"), |_| {}), held);
        let KeyRoute::Encode(input) = route else {
            panic!("expected encode, got {route:?}");
        };
        assert_eq!(input.action, KeyAction::Repeat);
        assert_eq!(send(&ks("j", Some("j"), |_| {}), held), b"j");
    }

    #[test]
    fn cmd_chords_belong_to_the_app() {
        assert_eq!(
            route_key_down(&ks("c", None, |m| m.platform = true), ctx()),
            KeyRoute::Ignore
        );
        if cfg!(target_os = "macos") {
            assert_eq!(
                route_key_down(&ks("left", None, |m| m.platform = true), ctx()),
                KeyRoute::Bytes(b"\x01")
            );
            assert_eq!(
                route_key_down(&ks("backspace", None, |m| m.platform = true), ctx()),
                KeyRoute::Bytes(b"\x15")
            );
        }
    }

    #[test]
    fn composition_owns_keys() {
        let composing = RouteContext {
            composing: true,
            ..ctx()
        };
        assert_eq!(
            route_key_down(&ks("backspace", None, |_| {}), composing),
            KeyRoute::Ignore
        );
    }

    #[test]
    fn kitty_protocol_encodes_text_keys_with_their_key() {
        let mut backend = GhosttyBackend::new(80, 24).unwrap();
        // Report all keys as escape codes.
        backend.write(b"\x1b[>8u");
        let a = ks("a", Some("a"), |_| {});
        assert_eq!(
            backend.encode_key(&committed_text_input(&a, false, "a")),
            b"\x1b[97u"
        );
        let release = key_up_input(&a, false);
        assert!(backend.encode_key(&release).is_empty());
        backend.write(b"\x1b[=10u"); // report all + report events
        assert_eq!(backend.encode_key(&release), b"\x1b[97;1:3u");
    }
}
