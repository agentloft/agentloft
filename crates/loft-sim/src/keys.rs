//! Keys as the simulator's keyboard sends them: USB HID usages (keyboard
//! page) for a US layout, which is what a simulator's hardware keyboard is
//! unless the user changed it in the device's settings.

/// Modifier bits as [`loft_protocol::page_modifiers`] numbers them, and the
/// left-hand key that makes each.
pub const MODIFIERS: [(u8, u32); 4] = [
    (loft_protocol::page_modifiers::CONTROL, 0xe0),
    (loft_protocol::page_modifiers::SHIFT, 0xe1),
    (loft_protocol::page_modifiers::ALT, 0xe2),
    (loft_protocol::page_modifiers::META, 0xe3),
];

pub const LEFT_SHIFT: u32 = 0xe1;
pub const LEFT_GUI: u32 = 0xe3;
pub const ENTER: u32 = 0x28;
pub const V: u32 = 0x19;

/// One key press that types a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub usage: u32,
    pub shift: bool,
}

/// The key (and whether Shift is held) that types `c`, if a US keyboard
/// has one.
pub fn stroke(c: char) -> Option<Stroke> {
    let plain = |usage| {
        Some(Stroke {
            usage,
            shift: false,
        })
    };
    let shifted = |usage| Some(Stroke { usage, shift: true });
    match c {
        'a'..='z' => plain(0x04 + (c as u32 - 'a' as u32)),
        'A'..='Z' => shifted(0x04 + (c as u32 - 'A' as u32)),
        '1'..='9' => plain(0x1e + (c as u32 - '1' as u32)),
        '0' => plain(0x27),
        '\n' | '\r' => plain(ENTER),
        '\t' => plain(0x2b),
        ' ' => plain(0x2c),
        '-' => plain(0x2d),
        '=' => plain(0x2e),
        '[' => plain(0x2f),
        ']' => plain(0x30),
        '\\' => plain(0x31),
        ';' => plain(0x33),
        '\'' => plain(0x34),
        '`' => plain(0x35),
        ',' => plain(0x36),
        '.' => plain(0x37),
        '/' => plain(0x38),
        '!' => shifted(0x1e),
        '@' => shifted(0x1f),
        '#' => shifted(0x20),
        '$' => shifted(0x21),
        '%' => shifted(0x22),
        '^' => shifted(0x23),
        '&' => shifted(0x24),
        '*' => shifted(0x25),
        '(' => shifted(0x26),
        ')' => shifted(0x27),
        '_' => shifted(0x2d),
        '+' => shifted(0x2e),
        '{' => shifted(0x2f),
        '}' => shifted(0x30),
        '|' => shifted(0x31),
        ':' => shifted(0x33),
        '"' => shifted(0x34),
        '~' => shifted(0x35),
        '<' => shifted(0x36),
        '>' => shifted(0x37),
        '?' => shifted(0x38),
        _ => None,
    }
}

/// A key without text, named as a DOM `KeyboardEvent.key`, or a single
/// character (for shortcuts such as ⌘A).
pub fn named(key: &str) -> Option<Stroke> {
    let usage = match key {
        "Enter" => ENTER,
        "Escape" => 0x29,
        "Backspace" => 0x2a,
        "Tab" => 0x2b,
        " " | "Space" | "Spacebar" => 0x2c,
        "CapsLock" => 0x39,
        "Delete" => 0x4c,
        "Insert" => 0x49,
        "Home" => 0x4a,
        "PageUp" => 0x4b,
        "End" => 0x4d,
        "PageDown" => 0x4e,
        "ArrowRight" => 0x4f,
        "ArrowLeft" => 0x50,
        "ArrowDown" => 0x51,
        "ArrowUp" => 0x52,
        _ => {
            if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok())
                && (1..=12).contains(&n)
            {
                0x3a + n - 1
            } else {
                let mut chars = key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                return stroke(c);
            }
        }
    };
    Some(Stroke {
        usage,
        shift: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_and_their_keys() {
        assert_eq!(
            stroke('a'),
            Some(Stroke {
                usage: 0x04,
                shift: false
            })
        );
        assert_eq!(
            stroke('Z'),
            Some(Stroke {
                usage: 0x1d,
                shift: true
            })
        );
        assert_eq!(stroke('0').unwrap().usage, 0x27);
        assert_eq!(stroke('9').unwrap().usage, 0x26);
        assert_eq!(
            stroke('?'),
            Some(Stroke {
                usage: 0x38,
                shift: true
            })
        );
        assert_eq!(stroke('é'), None);
        assert_eq!(stroke('\n').unwrap().usage, ENTER);
    }

    #[test]
    fn named_keys() {
        assert_eq!(named("Backspace").unwrap().usage, 0x2a);
        assert_eq!(named("ArrowUp").unwrap().usage, 0x52);
        assert_eq!(named("F1").unwrap().usage, 0x3a);
        assert_eq!(named("F12").unwrap().usage, 0x45);
        assert_eq!(named("a").unwrap().usage, 0x04);
        assert_eq!(named("F13"), None);
        assert_eq!(named("Unidentified"), None);
    }
}
