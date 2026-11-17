//! The VT engine boundary.
//!
//! [`TerminalBackend`] is everything the widget needs from a terminal
//! emulator: parse PTY output, keep scrollback and modes, expose a
//! render-ready grid with damage information, and encode input (keys, mouse,
//! paste, focus) against the modes the running program set. The view and the
//! element only ever talk to this trait, so the engine can be swapped
//! (ADR-0007 names `alacritty_terminal` as the alternative) without touching
//! rendering or input code.
//!
//! The data types here are engine-agnostic: colors are already resolved to
//! RGB, wide characters are explicit, and key names follow GPUI's
//! [`Keystroke::key`](gpui_kit::Keystroke) spelling ("a", "enter", "left",
//! "f5", ...).

pub(crate) mod ghostty;

/// An sRGB color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const fn hex(value: u32) -> Self {
        Self {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
    }

    /// Linear blend toward `other` by `t` (0 = self, 1 = other).
    pub fn blend(self, other: Rgb, t: f32) -> Rgb {
        let mix = |a: u8, b: u8| -> u8 {
            (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
                .round()
                .clamp(0., 255.) as u8
        };
        Rgb::new(
            mix(self.r, other.r),
            mix(self.g, other.g),
            mix(self.b, other.b),
        )
    }
}

/// How many columns a cell occupies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellWidth {
    /// A normal single-column cell.
    #[default]
    Narrow,
    /// The first column of a double-width character (CJK, most emoji).
    Wide,
    /// The second column of a wide character. Carries no text.
    SpacerTail,
    /// Placeholder at the end of a line when a wide character wrapped.
    SpacerHead,
}

/// Underline decoration (SGR 4 and its 4:x sub-parameters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Text attributes of one cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
}

/// One grid cell, with colors resolved through the active palette.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// The base character, `'\0'` for an empty cell.
    pub ch: char,
    /// Combining marks / the rest of a grapheme cluster, if any.
    pub extra: Option<Box<str>>,
    /// Explicit foreground; `None` means the terminal default.
    pub fg: Option<Rgb>,
    /// Explicit background; `None` means the terminal default.
    pub bg: Option<Rgb>,
    /// Explicit underline color (SGR 58); `None` follows the foreground.
    pub underline_color: Option<Rgb>,
    pub attrs: Attrs,
    pub width: CellWidth,
}

impl Cell {
    /// Whether the cell has printable text.
    pub fn has_text(&self) -> bool {
        (self.ch != '\0' && self.ch != ' ') || self.extra.is_some()
    }

    /// The cell's text (base character plus any combining marks).
    pub fn text(&self) -> String {
        let mut s = String::new();
        self.push_text(&mut s);
        s
    }

    /// Append the cell's text, rendering an empty cell as a space.
    pub fn push_text(&self, out: &mut String) {
        out.push(if self.ch == '\0' { ' ' } else { self.ch });
        if let Some(extra) = &self.extra {
            out.push_str(extra);
        }
    }
}

/// One viewport row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// Selected column range (inclusive) on this row, if any.
    pub selection: Option<(u16, u16)>,
}

impl Line {
    /// The row's text with trailing blanks trimmed (wide-char spacers skipped).
    pub fn text(&self) -> String {
        let mut s = String::new();
        for cell in &self.cells {
            if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                continue;
            }
            cell.push_text(&mut s);
        }
        s.truncate(s.trim_end().len());
        s
    }
}

/// Cursor shape requested by the program (DECSCUSR) or the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    BlockHollow,
    Bar,
    Underline,
}

/// Cursor state for the current viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Viewport position `(col, row)` when the cursor is on screen.
    pub position: Option<(u16, u16)>,
    /// Whether the cursor sits on a wide character (draw it two cells wide).
    pub wide: bool,
    pub visible: bool,
    pub blinking: bool,
    pub shape: CursorShape,
}

/// The render-ready state of the viewport.
///
/// The backend refreshes only the rows it reports as damaged; consumers keep
/// per-row caches keyed by row index and rebuild just those rows.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<Line>,
    pub cursor: Cursor,
    /// Effective default colors (theme defaults or OSC 10/11 overrides).
    pub foreground: Rgb,
    pub background: Rgb,
    /// Explicit cursor color (OSC 12), if the program set one.
    pub cursor_color: Option<Rgb>,
}

impl Frame {
    /// The text of viewport row `row`, trailing blanks trimmed.
    pub fn row_text(&self, row: usize) -> String {
        self.lines.get(row).map(Line::text).unwrap_or_default()
    }
}

/// Which rows changed in the last [`TerminalBackend::update_frame`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Damage {
    /// Nothing changed.
    #[default]
    None,
    /// Only these viewport rows changed (the cursor may also have moved).
    Rows(Vec<u16>),
    /// Everything must be redrawn.
    Full,
}

impl Damage {
    pub fn is_none(&self) -> bool {
        matches!(self, Damage::None)
    }
}

/// Side effects produced while parsing output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Bytes to write back to the PTY (query responses, mode reports).
    PtyWrite(Vec<u8>),
    Bell,
    TitleChanged(String),
    PwdChanged(String),
    /// OSC 52 (or similar) clipboard write from the running program.
    ClipboardWrite(String),
}

/// Terminal modes the widget needs to route input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    /// Any mouse tracking mode (X10, normal, button, any-event) is on.
    pub mouse_tracking: bool,
    /// Bracketed paste (DECSET 2004).
    pub bracketed_paste: bool,
    /// The alternate screen is active.
    pub alt_screen: bool,
    /// Alternate scroll (DECSET 1007): wheel sends arrows on the alt screen.
    pub alternate_scroll: bool,
    /// Focus reporting (DECSET 1004).
    pub focus_events: bool,
}

/// Scrollback position of the viewport, in rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

/// Viewport scroll request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Positive scrolls toward newer output (down), negative toward history.
    Delta(isize),
    Top,
    Bottom,
}

/// Key event phase.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

/// Keyboard modifiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Command on macOS, Super/Windows elsewhere.
    pub super_key: bool,
    pub caps_lock: bool,
}

/// A key event to encode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInput {
    /// Physical key, spelled like GPUI's `Keystroke::key` ("a", "enter",
    /// "left", "f5", "space", ...).
    pub key: String,
    pub action: KeyAction,
    pub mods: Mods,
    /// Modifiers the platform already applied to produce `text` (for
    /// example Shift in "A" or Option in "ß").
    pub consumed: Mods,
    /// The text the key produces in the current layout, without any
    /// Ctrl/Alt transformation. `None` for non-text keys.
    pub text: Option<String>,
    /// Whether an IME composition is in progress.
    pub composing: bool,
}

/// Mouse buttons the terminal can report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// Mouse event phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// Grid geometry in device pixels, used for mouse reports and selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl Geometry {
    pub fn width(self) -> u32 {
        u32::from(self.cols) * self.cell_width
    }

    pub fn height(self) -> u32 {
        u32::from(self.rows) * self.cell_height
    }
}

/// A mouse event to report to the running program.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    pub mods: Mods,
    /// Position in device pixels relative to the grid origin.
    pub x: f32,
    pub y: f32,
    /// Whether any button is currently held (for button-event tracking).
    pub any_button_pressed: bool,
}

/// A point on the grid for selection gestures: viewport cell plus the
/// pointer position in device pixels relative to the grid origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPoint {
    pub col: u16,
    pub row: u16,
    pub x: f32,
    pub y: f32,
}

/// The theme colors pushed into the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// The 16 ANSI colors; the 256-color cube and grays are standard.
    pub ansi: [Rgb; 16],
}

/// The VT engine behind the widget. See the module docs.
pub trait TerminalBackend {
    /// Parse PTY output.
    fn write(&mut self, bytes: &[u8]);

    /// Take the side effects queued by [`TerminalBackend::write`].
    fn take_effects(&mut self) -> Vec<Effect>;

    /// Full reset (RIS): clears screen, scrollback and modes.
    fn reset(&mut self);

    /// Resize the grid. Cell sizes are in device pixels.
    fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32);

    /// Current grid size `(cols, rows)`.
    fn size(&self) -> (u16, u16);

    /// The title set by OSC 0/2, if any.
    fn title(&self) -> Option<&str>;

    /// The working directory reported by OSC 7, if any.
    fn pwd(&self) -> Option<&str>;

    fn modes(&self) -> Modes;

    /// Whether synchronized output (DECSET 2026) is holding rendering.
    fn is_render_held(&self) -> bool;

    /// Treat macOS Option as Alt when encoding keys.
    fn set_option_as_alt(&mut self, enabled: bool);

    /// Set the default colors and the ANSI palette.
    fn set_colors(&mut self, colors: &Colors);

    /// Refresh `frame` from the engine, rewriting only damaged rows, and
    /// report what changed. `force_full` treats every row as damaged.
    fn update_frame(&mut self, frame: &mut Frame, force_full: bool) -> Damage;

    fn scroll(&mut self, scroll: Scroll);

    /// Whether the viewport shows the live screen (not scrolled back).
    fn is_at_bottom(&self) -> bool;

    fn scrollbar(&self) -> Scrollbar;

    /// Encode a key event; empty when the key produces nothing.
    fn encode_key(&mut self, key: &KeyInput) -> Vec<u8>;

    /// Encode a mouse event under the active tracking mode and format;
    /// empty when the mode does not report it.
    fn encode_mouse(&mut self, input: &MouseInput, geometry: Geometry) -> Vec<u8>;

    /// Encode pasted text (bracketed when enabled, unsafe bytes stripped).
    fn encode_paste(&mut self, text: &str) -> Vec<u8>;

    /// Encode a focus change if focus reporting is on.
    fn encode_focus(&mut self, focused: bool) -> Vec<u8>;

    /// Selection: start a click (`click_count` handled by the engine from
    /// timing and distance). `time` is a monotonic timestamp.
    fn selection_press(&mut self, point: GridPoint, time: std::time::Duration);

    /// Selection: drag to `point`; `rectangle` selects a block.
    fn selection_drag(&mut self, point: GridPoint, geometry: Geometry, rectangle: bool);

    /// Selection: end the gesture.
    fn selection_release(&mut self, point: Option<GridPoint>);

    fn select_all(&mut self);

    fn clear_selection(&mut self);

    fn has_selection(&self) -> bool;

    /// The selected text, soft wraps joined and trailing blanks trimmed.
    fn selection_text(&self) -> Option<String>;
}

#[cfg(test)]
mod tests;
