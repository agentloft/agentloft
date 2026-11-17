//! Keys for the DevTools protocol: the Windows virtual-key code Chrome wants
//! with every key event, and, on a Mac, the editing commands that the
//! system's key bindings would have produced (a headless page gets none of
//! those unless they are named).

use loft_protocol::page_modifiers::{ALT, CONTROL, META, SHIFT};

/// The virtual-key code for a DOM `code` ("KeyA", "Enter"…), falling back
/// on the `key` for letters and digits. 0 when unknown.
pub fn virtual_key(code: &str, key: &str) -> u32 {
    if let Some(letter) = code.strip_prefix("Key")
        && let Some(c) = single(letter)
        && c.is_ascii_alphabetic()
    {
        return c.to_ascii_uppercase() as u32;
    }
    if let Some(digit) = code.strip_prefix("Digit")
        && let Some(c) = single(digit)
        && c.is_ascii_digit()
    {
        return c as u32;
    }
    if let Some(n) = code.strip_prefix('F')
        && let Ok(n) = n.parse::<u32>()
        && (1..=24).contains(&n)
    {
        return 111 + n;
    }
    match code {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" | "NumpadEnter" => 13,
        "ShiftLeft" | "ShiftRight" => 16,
        "ControlLeft" | "ControlRight" => 17,
        "AltLeft" | "AltRight" => 18,
        "Escape" => 27,
        "Space" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Insert" => 45,
        "Delete" => 46,
        "MetaLeft" | "MetaRight" => 91,
        "Semicolon" => 186,
        "Equal" => 187,
        "Comma" => 188,
        "Minus" => 189,
        "Period" => 190,
        "Slash" => 191,
        "Backquote" => 192,
        "BracketLeft" => 219,
        "Backslash" => 220,
        "BracketRight" => 221,
        "Quote" => 222,
        _ => match single(key) {
            Some(c) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as u32,
            _ => 0,
        },
    }
}

fn single(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// The macOS editing command a key chord stands for, if any.
pub fn mac_command(key: &str, modifiers: u8) -> Option<&'static str> {
    let shift = modifiers & SHIFT != 0;
    let chord = modifiers & (ALT | CONTROL | META);
    let lower = key.to_ascii_lowercase();
    Some(match (chord, lower.as_str(), shift) {
        (META, "a", false) => "selectAll",
        (META, "z", false) => "undo",
        (META, "z", true) => "redo",
        (META, "arrowleft", false) => "moveToBeginningOfLine",
        (META, "arrowleft", true) => "moveToBeginningOfLineAndModifySelection",
        (META, "arrowright", false) => "moveToEndOfLine",
        (META, "arrowright", true) => "moveToEndOfLineAndModifySelection",
        (META, "arrowup", false) => "moveToBeginningOfDocument",
        (META, "arrowup", true) => "moveToBeginningOfDocumentAndModifySelection",
        (META, "arrowdown", false) => "moveToEndOfDocument",
        (META, "arrowdown", true) => "moveToEndOfDocumentAndModifySelection",
        (META, "backspace", _) => "deleteToBeginningOfLine",
        (ALT, "arrowleft", false) => "moveWordLeft",
        (ALT, "arrowleft", true) => "moveWordLeftAndModifySelection",
        (ALT, "arrowright", false) => "moveWordRight",
        (ALT, "arrowright", true) => "moveWordRightAndModifySelection",
        (ALT, "backspace", _) => "deleteWordBackward",
        (ALT, "delete", _) => "deleteWordForward",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_keys() {
        assert_eq!(virtual_key("KeyA", "a"), 65);
        assert_eq!(virtual_key("KeyZ", "Z"), 90);
        assert_eq!(virtual_key("Digit7", "7"), 55);
        assert_eq!(virtual_key("Enter", "Enter"), 13);
        assert_eq!(virtual_key("ArrowDown", "ArrowDown"), 40);
        assert_eq!(virtual_key("F5", "F5"), 116);
        assert_eq!(virtual_key("Slash", "/"), 191);
        // An unknown code still gets a letter's code from its key.
        assert_eq!(virtual_key("", "q"), 81);
        assert_eq!(virtual_key("", "é"), 0);
    }

    #[test]
    fn mac_editing_commands() {
        assert_eq!(mac_command("a", META), Some("selectAll"));
        assert_eq!(mac_command("Z", META | SHIFT), Some("redo"));
        assert_eq!(
            mac_command("ArrowLeft", ALT | SHIFT),
            Some("moveWordLeftAndModifySelection")
        );
        // Plain keys and copy/paste (the viewer's own clipboard) have none.
        assert_eq!(mac_command("a", 0), None);
        assert_eq!(mac_command("c", META), None);
        assert_eq!(mac_command("v", META), None);
    }
}
