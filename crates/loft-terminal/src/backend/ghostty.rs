//! [`TerminalBackend`] on Ghostty's libghostty-vt (through the `ghostty-vt`
//! crate).
//!
//! Rendering follows libghostty's render-state contract: one
//! `RenderState::update` per frame, then only the rows the state reports as
//! dirty are read back, and both dirty layers (global and per-row) are
//! cleared afterwards. Input is encoded by libghostty's own key, mouse, paste
//! and focus encoders, synced from the live terminal modes before each use,
//! so encoding always matches what the running program asked for (DECCKM,
//! kitty keyboard flags, SGR mouse, bracketed paste, ...).
//!
//! Structure adapted from Ghostex's `ghostty_vt` wrapper and
//! `terminal_model.rs` (MIT, see THIRD_PARTY_NOTICES.md).

use super::{
    Attrs, Cell, CellWidth, Colors, Cursor, CursorShape, Damage, Effect, Frame, Geometry,
    GridPoint, KeyAction, KeyInput, Line, Modes, Mods, MouseAction, MouseButton, MouseInput, Rgb,
    Scroll, Scrollbar, TerminalBackend, Underline,
};
use ghostty_vt::render::{CellIteration, CellIterator, CursorVisualStyle, Dirty, RowIterator};
use ghostty_vt::screen::{CellContentTag, CellWide, Screen};
use ghostty_vt::selection::FormatOptions;
use ghostty_vt::selection::gesture::{
    DragEvent, Geometry as GestureGeometry, Gesture, PressEvent, ReleaseEvent,
};
use ghostty_vt::style::{Palette, PaletteIndex, RgbColor, StyleColor};
use ghostty_vt::terminal::{Mode, Point, PointCoordinate, ScrollViewport};
use ghostty_vt::{RenderState, Terminal, TerminalOptions, focus, key, mouse, paste};
use std::time::Duration;
use web_time::Instant;

/// Scrollback budget in bytes (Ghostty's default `scrollback-limit`).
const SCROLLBACK_BYTES: usize = 10_000_000;
/// Longest a synchronized-output update (DECSET 2026) may hold rendering,
/// as in Ghostty.
const SYNC_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
/// Double/triple click window and slop for the selection gesture.
const REPEAT_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const REPEAT_CLICK_DISTANCE_PX: f64 = 8.;

pub(crate) struct GhosttyBackend {
    terminal: Terminal,
    render: RenderState,
    rows: RowIterator,
    cells: CellIterator,
    key_encoder: key::Encoder,
    key_event: key::Event,
    mouse_encoder: mouse::Encoder,
    mouse_event: mouse::Event,
    /// Mouse modes and geometry the encoder was last synced with. Syncing
    /// resets the encoder's last-reported-cell dedupe, so it only happens
    /// when one of them changes.
    mouse_sync: Option<([bool; 8], Geometry)>,
    gesture: Gesture,
    press: PressEvent,
    drag: DragEvent,
    release: ReleaseEvent,
    /// Treat macOS Option as Alt when encoding keys.
    option_as_alt: bool,
    palette: [Rgb; 256],
    grapheme_buf: Vec<char>,
    title: Option<String>,
    pwd: Option<String>,
    effects: Vec<Effect>,
    sync_since: Option<Instant>,
}

impl GhosttyBackend {
    pub(crate) fn new(cols: u16, rows: u16) -> Result<Self, ghostty_vt::Error> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: cols.max(1),
            rows: rows.max(1),
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        // Ask before letting programs touch the system clipboard? Ghostty's
        // default is to allow OSC 52 writes; reads are never forwarded.
        terminal.set_xtversion(Some(format!("agentloft {}", env!("CARGO_PKG_VERSION"))));
        let mut press = PressEvent::new()?;
        press
            .set_repeat_interval(REPEAT_CLICK_INTERVAL)?
            .set_repeat_distance(REPEAT_CLICK_DISTANCE_PX)?;
        Ok(Self {
            terminal,
            render: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            mouse_sync: None,
            gesture: Gesture::new()?,
            press,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            option_as_alt: !cfg!(target_os = "macos"),
            palette: Palette::default().0.map(rgb_from),
            grapheme_buf: Vec::with_capacity(8),
            title: None,
            pwd: None,
            effects: Vec::new(),
            sync_since: None,
        })
    }

    fn mode(&self, mode: Mode) -> bool {
        self.terminal.mode(mode).unwrap_or(false)
    }

    fn drain_engine_events(&mut self) {
        for event in self.terminal.events() {
            let effect = match event {
                ghostty_vt::Event::PtyWrite(bytes) => Effect::PtyWrite(bytes),
                ghostty_vt::Event::Bell => Effect::Bell,
                ghostty_vt::Event::TitleChanged(title) => {
                    self.title = (!title.is_empty()).then(|| title.clone());
                    Effect::TitleChanged(title)
                }
                ghostty_vt::Event::PwdChanged(pwd) => {
                    self.pwd = (!pwd.is_empty()).then(|| pwd.clone());
                    Effect::PwdChanged(pwd)
                }
                ghostty_vt::Event::ClipboardWrite(write) => {
                    let text = write
                        .contents
                        .iter()
                        .find(|c| c.mime.starts_with("text/plain") || c.mime.is_empty())
                        .or_else(|| write.contents.first())
                        .map(|c| String::from_utf8_lossy(&c.data).into_owned());
                    match text {
                        Some(text) => Effect::ClipboardWrite(text),
                        None => continue,
                    }
                }
                _ => continue,
            };
            self.effects.push(effect);
        }
    }

    /// Whether synchronized output currently holds rendering.
    fn render_held(&mut self) -> bool {
        if !self.mode(Mode::SYNC_OUTPUT) {
            self.sync_since = None;
            return false;
        }
        let since = *self.sync_since.get_or_insert_with(Instant::now);
        if since.elapsed() < SYNC_OUTPUT_TIMEOUT {
            return true;
        }
        let _ = self.terminal.set_mode(Mode::SYNC_OUTPUT, false);
        self.sync_since = None;
        false
    }

    fn grid_point(&self, point: GridPoint) -> Point {
        let (cols, rows) = self.size();
        Point::Viewport(PointCoordinate {
            x: point.col.min(cols.saturating_sub(1)),
            y: u32::from(point.row.min(rows.saturating_sub(1))),
        })
    }
}

impl TerminalBackend for GhosttyBackend {
    fn write(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
        if self.terminal.has_events() {
            self.drain_engine_events();
        }
    }

    fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn reset(&mut self) {
        self.terminal.reset();
        self.gesture.reset(&self.terminal);
        let _ = self.terminal.set_selection(None);
        self.title = None;
        self.pwd = None;
        self.effects.clear();
        self.sync_since = None;
        self.mouse_sync = None;
    }

    fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        if let Err(err) = self
            .terminal
            .resize(cols.max(1), rows.max(1), cell_width, cell_height)
        {
            tracing::warn!(?err, cols, rows, "terminal resize failed");
        }
        if self.terminal.has_events() {
            self.drain_engine_events();
        }
    }

    fn size(&self) -> (u16, u16) {
        (
            self.terminal.cols().unwrap_or(1),
            self.terminal.rows().unwrap_or(1),
        )
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn pwd(&self) -> Option<&str> {
        self.pwd.as_deref()
    }

    fn modes(&self) -> Modes {
        Modes {
            mouse_tracking: self.terminal.is_mouse_tracking().unwrap_or(false),
            bracketed_paste: self.mode(Mode::BRACKETED_PASTE),
            alt_screen: self.terminal.active_screen().ok() == Some(Screen::Alternate),
            alternate_scroll: self.mode(Mode::ALT_SCROLL),
            focus_events: self.mode(Mode::FOCUS_EVENT),
        }
    }

    fn is_render_held(&self) -> bool {
        self.mode(Mode::SYNC_OUTPUT)
    }

    fn set_option_as_alt(&mut self, enabled: bool) {
        self.option_as_alt = enabled;
    }

    fn set_colors(&mut self, colors: &Colors) {
        let mut palette = Palette::default();
        for (index, color) in colors.ansi.iter().enumerate() {
            palette.0[index] = rgb_to(*color);
        }
        let result = self
            .terminal
            .set_default_fg_color(Some(rgb_to(colors.foreground)))
            .and_then(|t| t.set_default_bg_color(Some(rgb_to(colors.background))))
            .and_then(|t| t.set_default_cursor_color(Some(rgb_to(colors.cursor))))
            .and_then(|t| t.set_default_color_palette(Some(palette)));
        if let Err(err) = result {
            tracing::warn!(?err, "failed to apply terminal colors");
        }
    }

    fn update_frame(&mut self, frame: &mut Frame, force_full: bool) -> Damage {
        if self.render_held() && !force_full {
            return Damage::None;
        }
        let Ok(snapshot) = self.render.update(&self.terminal) else {
            return Damage::None;
        };
        let cols = snapshot.cols().unwrap_or(frame.cols);
        let rows = snapshot.rows().unwrap_or(frame.rows);
        let mut full = force_full;
        if frame.cols != cols || frame.rows != rows || frame.lines.len() != usize::from(rows) {
            frame.cols = cols;
            frame.rows = rows;
            frame.lines.resize_with(usize::from(rows), Line::default);
            full = true;
        }
        if let Ok(colors) = snapshot.colors() {
            let foreground = rgb_from(colors.foreground);
            let background = rgb_from(colors.background);
            let palette = colors.palette.map(rgb_from);
            if foreground != frame.foreground
                || background != frame.background
                || palette != self.palette
            {
                full = true;
            }
            frame.foreground = foreground;
            frame.background = background;
            frame.cursor_color = colors.cursor.map(rgb_from);
            self.palette = palette;
        }

        let previous_cursor = frame.cursor;
        let dirty = snapshot.dirty().unwrap_or(Dirty::Full);
        if full {
            let _ = snapshot.set_dirty(Dirty::Full);
        }
        let mut damaged = Vec::new();
        if full || dirty != Dirty::Clean {
            let Ok(mut row_iter) = self.rows.update(&snapshot) else {
                return Damage::Full;
            };
            while let Some((y, row)) = row_iter.next_dirty() {
                let Some(line) = frame.lines.get_mut(usize::from(y)) else {
                    continue;
                };
                line.selection = row
                    .selection()
                    .ok()
                    .flatten()
                    .map(|sel| (sel.start_x, sel.end_x));
                line.cells.clear();
                if let Ok(mut cell_iter) = self.cells.update(row) {
                    while let Some(cell) = cell_iter.next() {
                        line.cells
                            .push(read_cell(cell, &self.palette, &mut self.grapheme_buf));
                    }
                }
                line.cells.resize(usize::from(cols), Cell::default());
                let _ = row.set_dirty(false);
                damaged.push(y);
            }
        }
        frame.cursor = read_cursor(&snapshot, frame);
        let _ = snapshot.clean();

        if full {
            Damage::Full
        } else if damaged.is_empty() && frame.cursor == previous_cursor {
            Damage::None
        } else {
            Damage::Rows(damaged)
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        self.terminal.scroll_viewport(match scroll {
            Scroll::Delta(rows) => ScrollViewport::Delta(rows),
            Scroll::Top => ScrollViewport::Top,
            Scroll::Bottom => ScrollViewport::Bottom,
        });
    }

    fn is_at_bottom(&self) -> bool {
        self.terminal.is_viewport_active().unwrap_or(true)
    }

    fn scrollbar(&self) -> Scrollbar {
        self.terminal
            .scrollbar()
            .map(|s| Scrollbar {
                total: s.total,
                offset: s.offset,
                len: s.len,
            })
            .unwrap_or_default()
    }

    fn encode_key(&mut self, input: &KeyInput) -> Vec<u8> {
        let (vt_key, unshifted) = vt_key(&input.key);
        self.key_encoder
            .set_options_from_terminal(&self.terminal)
            .set_macos_option_as_alt(if self.option_as_alt {
                key::OptionAsAlt::True
            } else {
                key::OptionAsAlt::False
            });
        self.key_event
            .set_action(match input.action {
                KeyAction::Press => key::Action::Press,
                KeyAction::Repeat => key::Action::Repeat,
                KeyAction::Release => key::Action::Release,
            })
            .set_key(vt_key)
            .set_mods(vt_mods(input.mods))
            .set_consumed_mods(vt_mods(input.consumed))
            .set_composing(input.composing)
            .set_utf8(input.text.as_deref().filter(|t| is_encodable_text(t)))
            .set_unshifted_codepoint(unshifted.unwrap_or('\0'));
        let mut out = Vec::new();
        if let Err(err) = self.key_encoder.encode_to_vec(&self.key_event, &mut out) {
            tracing::debug!(?err, key = input.key, "key encode failed");
            out.clear();
        }
        out
    }

    fn encode_mouse(&mut self, input: &MouseInput, geometry: Geometry) -> Vec<u8> {
        let modes = [
            Mode::X10_MOUSE,
            Mode::NORMAL_MOUSE,
            Mode::BUTTON_MOUSE,
            Mode::ANY_MOUSE,
            Mode::UTF8_MOUSE,
            Mode::SGR_MOUSE,
            Mode::URXVT_MOUSE,
            Mode::SGR_PIXELS_MOUSE,
        ]
        .map(|m| self.mode(m));
        if self.mouse_sync != Some((modes, geometry)) {
            self.mouse_encoder
                .set_options_from_terminal(&self.terminal)
                .set_size(mouse::EncoderSize {
                    screen_width: geometry.width(),
                    screen_height: geometry.height(),
                    cell_width: geometry.cell_width.max(1),
                    cell_height: geometry.cell_height.max(1),
                    padding_top: 0,
                    padding_bottom: 0,
                    padding_right: 0,
                    padding_left: 0,
                })
                .set_track_last_cell(true);
            self.mouse_sync = Some((modes, geometry));
        }
        self.mouse_encoder
            .set_any_button_pressed(input.any_button_pressed);
        self.mouse_event
            .set_action(match input.action {
                MouseAction::Press => mouse::Action::Press,
                MouseAction::Release => mouse::Action::Release,
                MouseAction::Motion => mouse::Action::Motion,
            })
            .set_button(input.button.map(|b| match b {
                MouseButton::Left => mouse::Button::Left,
                MouseButton::Middle => mouse::Button::Middle,
                MouseButton::Right => mouse::Button::Right,
                MouseButton::WheelUp => mouse::Button::Four,
                MouseButton::WheelDown => mouse::Button::Five,
            }))
            .set_mods(vt_mods(input.mods))
            .set_position(mouse::Position {
                x: input.x,
                y: input.y,
            });
        let mut out = Vec::new();
        if self
            .mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut out)
            .is_err()
        {
            out.clear();
        }
        out
    }

    fn encode_paste(&mut self, text: &str) -> Vec<u8> {
        let bracketed = self.mode(Mode::BRACKETED_PASTE);
        let mut data = text.as_bytes().to_vec();
        let mut out = vec![0u8; data.len() + 16];
        match paste::encode(&mut data, bracketed, &mut out) {
            Ok(len) => {
                out.truncate(len);
                out
            }
            Err(ghostty_vt::Error::OutOfSpace { required }) => {
                let mut data = text.as_bytes().to_vec();
                out.resize(required, 0);
                match paste::encode(&mut data, bracketed, &mut out) {
                    Ok(len) => {
                        out.truncate(len);
                        out
                    }
                    Err(_) => Vec::new(),
                }
            }
            Err(_) => Vec::new(),
        }
    }

    fn encode_focus(&mut self, focused: bool) -> Vec<u8> {
        if !self.mode(Mode::FOCUS_EVENT) {
            return Vec::new();
        }
        let event = if focused {
            focus::Event::Gained
        } else {
            focus::Event::Lost
        };
        let mut buf = [0u8; 8];
        match event.encode(&mut buf) {
            Ok(len) => buf[..len].to_vec(),
            Err(_) => Vec::new(),
        }
    }

    fn selection_press(&mut self, point: GridPoint, time: Duration) {
        let grid_point = self.grid_point(point);
        let Ok(grid_ref) = self.terminal.grid_ref(grid_point) else {
            return;
        };
        let applied = self
            .press
            .set_position(f64::from(point.x), f64::from(point.y))
            .and_then(|p| p.set_time(time))
            .and_then(|p| p.apply(&mut self.gesture, &self.terminal, grid_ref));
        match applied {
            Ok(selection) => {
                let _ = self.terminal.set_selection(selection.as_ref());
            }
            Err(err) => tracing::debug!(?err, "selection press failed"),
        }
    }

    fn selection_drag(&mut self, point: GridPoint, geometry: Geometry, rectangle: bool) {
        let grid_point = self.grid_point(point);
        let Ok(grid_ref) = self.terminal.grid_ref(grid_point) else {
            return;
        };
        let geometry = GestureGeometry {
            columns: u32::from(geometry.cols.max(1)),
            cell_width: geometry.cell_width.max(1),
            padding_left: 0,
            screen_height: geometry.height().max(1),
        };
        let applied = self
            .drag
            .set_position(f64::from(point.x), f64::from(point.y))
            .and_then(|d| d.set_rectangle(rectangle))
            .and_then(|d| d.apply(&mut self.gesture, &self.terminal, grid_ref, geometry));
        match applied {
            Ok(Some(selection)) => {
                let _ = self.terminal.set_selection(Some(&selection));
            }
            Ok(None) => {}
            Err(err) => tracing::debug!(?err, "selection drag failed"),
        }
    }

    fn selection_release(&mut self, point: Option<GridPoint>) {
        let grid_ref = point
            .map(|p| self.grid_point(p))
            .and_then(|p| self.terminal.grid_ref(p).ok());
        let _ = self
            .release
            .apply(&mut self.gesture, &self.terminal, grid_ref);
    }

    fn select_all(&mut self) {
        if let Ok(Some(selection)) = self.terminal.select_all() {
            let _ = self.terminal.set_selection(Some(&selection));
        }
    }

    fn clear_selection(&mut self) {
        let _ = self.terminal.set_selection(None);
    }

    fn has_selection(&self) -> bool {
        matches!(self.terminal.selection(), Ok(Some(_)))
    }

    fn selection_text(&self) -> Option<String> {
        let bytes = self
            .terminal
            .format_selection_alloc(FormatOptions::new().with_unwrap(true).with_trim(true))
            .ok()??;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (!text.is_empty()).then_some(text)
    }
}

fn rgb_from(c: RgbColor) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

fn rgb_to(c: Rgb) -> RgbColor {
    RgbColor {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

fn read_cursor(snapshot: &ghostty_vt::render::Snapshot<'_>, frame: &Frame) -> Cursor {
    let Ok(cursor) = snapshot.cursor() else {
        return Cursor::default();
    };
    let shape = match cursor.visual_style {
        CursorVisualStyle::Bar => CursorShape::Bar,
        CursorVisualStyle::Underline => CursorShape::Underline,
        CursorVisualStyle::BlockHollow => CursorShape::BlockHollow,
        _ => CursorShape::Block,
    };
    let (position, wide) = match cursor.viewport {
        Some(vp) if vp.at_wide_tail => (Some((vp.x.saturating_sub(1), vp.y)), true),
        Some(vp) => {
            let wide = frame
                .lines
                .get(usize::from(vp.y))
                .and_then(|line| line.cells.get(usize::from(vp.x)))
                .is_some_and(|cell| cell.width == CellWidth::Wide);
            (Some((vp.x, vp.y)), wide)
        }
        None => (None, false),
    };
    Cursor {
        position,
        wide,
        visible: cursor.visible,
        blinking: cursor.blinking,
        shape,
    }
}

fn read_cell(cell: &CellIteration<'_>, palette: &[Rgb; 256], buf: &mut Vec<char>) -> Cell {
    let raw = cell.raw_cell().ok();
    let width = match raw.and_then(|r| r.wide().ok()) {
        Some(CellWide::Wide) => CellWidth::Wide,
        Some(CellWide::SpacerTail) => CellWidth::SpacerTail,
        Some(CellWide::SpacerHead) => CellWidth::SpacerHead,
        _ => CellWidth::Narrow,
    };
    let mut out = Cell {
        width,
        ..Cell::default()
    };

    let len = cell.graphemes_len().unwrap_or(0);
    if len > 0 {
        buf.clear();
        buf.resize(len, '\0');
        if cell.graphemes_buf(buf).is_ok() {
            out.ch = buf[0];
            if len > 1 {
                out.extra = Some(buf[1..].iter().collect::<String>().into_boxed_str());
            }
        }
    }

    let styled = cell.has_styling().unwrap_or(false);
    if styled {
        if let Ok(style) = cell.style() {
            out.attrs = Attrs {
                bold: style.bold,
                italic: style.italic,
                faint: style.faint,
                blink: style.blink,
                inverse: style.inverse,
                invisible: style.invisible,
                strikethrough: style.strikethrough,
                overline: style.overline,
                underline: match style.underline {
                    ghostty_vt::style::Underline::None => Underline::None,
                    ghostty_vt::style::Underline::Double => Underline::Double,
                    ghostty_vt::style::Underline::Curly => Underline::Curly,
                    ghostty_vt::style::Underline::Dotted => Underline::Dotted,
                    ghostty_vt::style::Underline::Dashed => Underline::Dashed,
                    _ => Underline::Single,
                },
            };
            out.underline_color = match style.underline_color {
                StyleColor::None => None,
                StyleColor::Palette(PaletteIndex(index)) => Some(palette[usize::from(index)]),
                StyleColor::Rgb(c) => Some(rgb_from(c)),
            };
        }
        out.fg = cell.fg_color().ok().flatten().map(rgb_from);
    }
    // Cells erased with a background color carry it without any style.
    let bg_only = matches!(
        raw.and_then(|r| r.content_tag().ok()),
        Some(CellContentTag::BgColorPalette | CellContentTag::BgColorRgb)
    );
    if styled || bg_only {
        out.bg = cell.bg_color().ok().flatten().map(rgb_from);
    }
    out
}

/// Text the key encoder may receive: no C0/DEL controls, no macOS private
/// function-key codepoints.
fn is_encodable_text(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c))
}

fn vt_mods(mods: Mods) -> key::Mods {
    let mut out = key::Mods::empty();
    if mods.shift {
        out |= key::Mods::SHIFT;
    }
    if mods.ctrl {
        out |= key::Mods::CTRL;
    }
    if mods.alt {
        out |= key::Mods::ALT;
    }
    if mods.super_key {
        out |= key::Mods::SUPER;
    }
    if mods.caps_lock {
        out |= key::Mods::CAPS_LOCK;
    }
    out
}

/// GPUI key name → libghostty physical key and unshifted codepoint.
fn vt_key(name: &str) -> (key::Key, Option<char>) {
    use key::Key as K;
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let k = match c.to_ascii_lowercase() {
            'a' => K::A,
            'b' => K::B,
            'c' => K::C,
            'd' => K::D,
            'e' => K::E,
            'f' => K::F,
            'g' => K::G,
            'h' => K::H,
            'i' => K::I,
            'j' => K::J,
            'k' => K::K,
            'l' => K::L,
            'm' => K::M,
            'n' => K::N,
            'o' => K::O,
            'p' => K::P,
            'q' => K::Q,
            'r' => K::R,
            's' => K::S,
            't' => K::T,
            'u' => K::U,
            'v' => K::V,
            'w' => K::W,
            'x' => K::X,
            'y' => K::Y,
            'z' => K::Z,
            '0' => K::Digit0,
            '1' => K::Digit1,
            '2' => K::Digit2,
            '3' => K::Digit3,
            '4' => K::Digit4,
            '5' => K::Digit5,
            '6' => K::Digit6,
            '7' => K::Digit7,
            '8' => K::Digit8,
            '9' => K::Digit9,
            '`' => K::Backquote,
            '\\' => K::Backslash,
            '[' => K::BracketLeft,
            ']' => K::BracketRight,
            ',' => K::Comma,
            '=' => K::Equal,
            '-' => K::Minus,
            '.' => K::Period,
            '\'' => K::Quote,
            ';' => K::Semicolon,
            '/' => K::Slash,
            ' ' => K::Space,
            _ => K::Unidentified,
        };
        return (k, Some(c.to_ascii_lowercase()));
    }
    let k = match name {
        "space" => K::Space,
        "tab" => K::Tab,
        "enter" => K::Enter,
        "backspace" => K::Backspace,
        "escape" => K::Escape,
        "up" => K::ArrowUp,
        "down" => K::ArrowDown,
        "left" => K::ArrowLeft,
        "right" => K::ArrowRight,
        "pageup" => K::PageUp,
        "pagedown" => K::PageDown,
        "home" => K::Home,
        "end" => K::End,
        "delete" => K::Delete,
        "insert" => K::Insert,
        "f1" => K::F1,
        "f2" => K::F2,
        "f3" => K::F3,
        "f4" => K::F4,
        "f5" => K::F5,
        "f6" => K::F6,
        "f7" => K::F7,
        "f8" => K::F8,
        "f9" => K::F9,
        "f10" => K::F10,
        "f11" => K::F11,
        "f12" => K::F12,
        "f13" => K::F13,
        "f14" => K::F14,
        "f15" => K::F15,
        "f16" => K::F16,
        "f17" => K::F17,
        "f18" => K::F18,
        "f19" => K::F19,
        "f20" => K::F20,
        "f21" => K::F21,
        "f22" => K::F22,
        "f23" => K::F23,
        "f24" => K::F24,
        "shift" => K::ShiftLeft,
        "control" => K::ControlLeft,
        "alt" => K::AltLeft,
        "platform" => K::MetaLeft,
        _ => K::Unidentified,
    };
    (k, (k == K::Space).then_some(' '))
}
