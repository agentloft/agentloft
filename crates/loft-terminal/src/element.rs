//! The GPUI element that draws a [`TerminalView`] and routes pointer input
//! back to it.
//!
//! Each viewport row is laid out once into a [`RowLayout`] (background
//! spans, shaped text runs, box-drawing shapes, overlines) and kept in the
//! view's `rows_cache` until the backend reports the row damaged. Everything
//! that changes without the row changing (cursor, selection, IME text,
//! scrollbar) is worked out per frame and drawn on top, so a blinking cursor
//! or a moving pointer costs no text shaping.

use crate::backend::{Cell, CellWidth, CursorShape, Frame, Rgb, Underline};
use crate::theme;
use crate::view::{Grid, TerminalView};
use gpui_kit::{
    App, BorderStyle, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, Focusable as _, Font, FontFeatures, FontStyle, FontWeight,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, ScrollEvent,
    ShapedLine, SharedString, Size, StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle,
    Window, fill, outline, point, px, relative, size,
};
use std::rc::Rc;
use std::sync::Arc;

/// Inner padding between the element edge and the grid.
pub(crate) const PADDING: Size<Pixels> = Size {
    width: px(8.),
    height: px(6.),
};

/// Faint text is the foreground this far toward the background.
const FAINT_BLEND: f32 = 0.45;
/// Box-drawing line weights, in logical pixels.
const LIGHT: f32 = 1.;
const HEAVY: f32 = 2.;
/// Scrollbar knob: width, inset of its left edge from the right edge,
/// minimum height, corner radius, opacity.
const KNOB_WIDTH: Pixels = px(4.);
const KNOB_INSET: Pixels = px(7.);
const KNOB_MIN_HEIGHT: Pixels = px(24.);
const KNOB_RADIUS: Pixels = px(2.);
const KNOB_ALPHA: f32 = 0.35;

/// The terminal font.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FontConfig {
    pub family: SharedString,
    pub size: Pixels,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "Menlo".into(),
            size: px(13.),
        }
    }
}

impl FontConfig {
    /// The font for a cell: one glyph per cell, so no ligatures.
    fn font(&self, bold: bool, italic: bool) -> Font {
        Font {
            family: self.family.clone(),
            features: FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 0)])),
            fallbacks: None,
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }
}

/// Size of one cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellMetrics {
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

impl CellMetrics {
    pub fn measure(font: &FontConfig, window: &mut Window) -> Self {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font.font(false, false));
        let advance = text_system
            .advance(font_id, font.size, 'm')
            .ok()
            .map(|advance| advance.width);
        let ascent = text_system.ascent(font_id, font.size);
        let descent = text_system.descent(font_id, font.size);
        Self::from_font(advance, ascent, descent, font.size, window.scale_factor())
    }

    /// Cell size from the font's numbers, with fallbacks for a text system
    /// that has none, rounded to whole device pixels.
    fn from_font(
        advance: Option<Pixels>,
        ascent: Pixels,
        descent: Pixels,
        font_size: Pixels,
        scale: f32,
    ) -> Self {
        let scale = scale.max(1.);
        let width = advance
            .map(f32::from)
            .filter(|w| *w >= 1.)
            .unwrap_or(f32::from(font_size) * 0.6);
        let height = f32::from(ascent) + f32::from(descent).abs();
        let height = if height < 1. {
            f32::from(font_size) * 1.2
        } else {
            height
        };
        Self {
            cell_width: px((width * scale).round().max(1.) / scale),
            line_height: px((height * scale).ceil().max(1.) / scale),
        }
    }

    pub fn device_cell_width(&self, scale: f32) -> u32 {
        device(self.cell_width, scale)
    }

    pub fn device_line_height(&self, scale: f32) -> u32 {
        device(self.line_height, scale)
    }
}

fn device(length: Pixels, scale: f32) -> u32 {
    (f32::from(length) * scale.max(1.)).round().max(1.) as u32
}

// ----- colours -----

/// A cell's foreground and (non-default) background after inverse and
/// faint.
fn resolve_colors(cell: &Cell, foreground: Rgb, background: Rgb) -> (Rgb, Option<Rgb>) {
    let mut fg = cell.fg.unwrap_or(foreground);
    let mut bg = cell.bg;
    if cell.attrs.inverse {
        let new_fg = bg.unwrap_or(background);
        bg = Some(fg);
        fg = new_fg;
    }
    if cell.attrs.faint {
        fg = fg.blend(bg.unwrap_or(background), FAINT_BLEND);
    }
    (fg, bg)
}

/// Neighbouring cells with the same background as one span:
/// `(first column, columns, colour)`.
fn background_spans(colors: impl IntoIterator<Item = Option<Rgb>>) -> Vec<(u16, u16, Rgb)> {
    let mut spans: Vec<(u16, u16, Rgb)> = Vec::new();
    for (col, color) in colors.into_iter().enumerate() {
        let Some(color) = color else { continue };
        let col = col as u16;
        match spans.last_mut() {
            Some((start, len, last)) if *last == color && *start + *len == col => *len += 1,
            _ => spans.push((col, 1, color)),
        }
    }
    spans
}

// ----- runs -----

/// What a run's cells must share.
#[derive(Clone, Debug, PartialEq)]
struct RunStyle {
    bold: bool,
    italic: bool,
    fg: Rgb,
    underline: Underline,
    underline_color: Rgb,
    strikethrough: bool,
}

impl RunStyle {
    fn decorated(&self) -> bool {
        self.underline != Underline::None || self.strikethrough
    }

    fn text_run(&self, font: &FontConfig, len: usize, color: Rgb) -> TextRun {
        TextRun {
            len,
            font: font.font(self.bold, self.italic),
            color: theme::to_hsla(color),
            background_color: None,
            underline: underline_style(self.underline, theme::to_hsla(self.underline_color)),
            strikethrough: self.strikethrough.then(|| StrikethroughStyle {
                thickness: px(1.),
                color: Some(theme::to_hsla(color)),
            }),
        }
    }
}

fn underline_style(underline: Underline, color: Hsla) -> Option<UnderlineStyle> {
    let (thickness, wavy) = match underline {
        Underline::None => return None,
        Underline::Single | Underline::Dotted | Underline::Dashed => (1., false),
        // Two lines approximated as one thick one (ADR-0007).
        Underline::Double => (2., false),
        Underline::Curly => (1., true),
    };
    Some(UnderlineStyle {
        thickness: px(thickness),
        color: Some(color),
        wavy,
    })
}

/// A piece of a row to draw as text.
#[derive(Debug, PartialEq)]
enum Piece {
    /// Cells shaped together, each forced to one column.
    Run {
        col: u16,
        cells: u16,
        text: String,
        style: RunStyle,
    },
    /// A double-width character, shaped alone at its natural width.
    Wide {
        col: u16,
        text: String,
        style: RunStyle,
    },
}

/// A run being collected.
struct OpenRun {
    col: u16,
    cells: u16,
    text: String,
    style: RunStyle,
    /// Length and cell count up to the last cell that isn't a plain blank,
    /// for cutting trailing spaces.
    kept: (usize, u16),
}

impl OpenRun {
    fn close(mut self, pieces: &mut Vec<Piece>) {
        let (len, cells) = self.kept;
        if cells == 0 {
            return;
        }
        self.text.truncate(len);
        pieces.push(Piece::Run {
            col: self.col,
            cells,
            text: self.text,
            style: self.style,
        });
    }
}

/// A cell drawn as shapes instead of a glyph.
fn is_shape_cell(cell: &Cell) -> bool {
    cell.width == CellWidth::Narrow
        && cell.extra.is_none()
        && !cell.attrs.invisible
        && cell.attrs.underline == Underline::None
        && !cell.attrs.strikethrough
        && is_shape_char(cell.ch)
}

/// Split a row into text pieces (Rule 1 and 2 of the spec): runs of
/// contiguous, identically styled cells, wide characters alone. Shape cells
/// and spacers end a run; plain blanks extend an undecorated one.
fn text_pieces(cells: &[Cell], foreground: Rgb, background: Rgb) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut open: Option<OpenRun> = None;
    for (col, cell) in cells.iter().enumerate() {
        let col = col as u16;
        if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead)
            || is_shape_cell(cell)
        {
            if let Some(run) = open.take() {
                run.close(&mut pieces);
            }
            continue;
        }
        let invisible = cell.attrs.invisible;
        let (fg, _) = resolve_colors(cell, foreground, background);
        let style = RunStyle {
            bold: cell.attrs.bold,
            italic: cell.attrs.italic,
            fg,
            underline: if invisible {
                Underline::None
            } else {
                cell.attrs.underline
            },
            underline_color: cell.underline_color.unwrap_or(fg),
            strikethrough: cell.attrs.strikethrough && !invisible,
        };
        let blank = !cell.has_text() || invisible;

        if cell.width == CellWidth::Wide {
            if let Some(run) = open.take() {
                run.close(&mut pieces);
            }
            if !blank {
                pieces.push(Piece::Wide {
                    col,
                    text: cell.text(),
                    style,
                });
            }
            continue;
        }

        if blank && !style.decorated() {
            // A space inside an undecorated run keeps it one shaping call.
            match &mut open {
                Some(run) if !run.style.decorated() => {
                    run.text.push(' ');
                    run.cells += 1;
                }
                _ => {
                    if let Some(run) = open.take() {
                        run.close(&mut pieces);
                    }
                }
            }
            continue;
        }

        let text = if blank { " ".to_owned() } else { cell.text() };
        match &mut open {
            Some(run) if run.style == style => {
                run.text.push_str(&text);
                run.cells += 1;
                run.kept = (run.text.len(), run.cells);
            }
            _ => {
                if let Some(run) = open.take() {
                    run.close(&mut pieces);
                }
                let len = text.len();
                open = Some(OpenRun {
                    col,
                    cells: 1,
                    text,
                    style,
                    kept: (len, 1),
                });
            }
        }
    }
    if let Some(run) = open.take() {
        run.close(&mut pieces);
    }
    pieces
}

// ----- box drawing and blocks -----

/// A shape in cell or row coordinates.
#[derive(Clone, Debug, PartialEq)]
enum Shape {
    Rect(Bounds<Pixels>),
    /// A quadratic curve stroked 1 px wide.
    Curve {
        from: Point<Pixels>,
        ctrl: Point<Pixels>,
        to: Point<Pixels>,
    },
}

impl Shape {
    fn offset(&self, by: Point<Pixels>) -> Shape {
        match self {
            Shape::Rect(bounds) => Shape::Rect(Bounds::new(bounds.origin + by, bounds.size)),
            Shape::Curve { from, ctrl, to } => Shape::Curve {
                from: *from + by,
                ctrl: *ctrl + by,
                to: *to + by,
            },
        }
    }
}

/// Arm weights (left, right, up, down) of the box-drawing lines drawn as
/// shapes; 1 light, 2 heavy.
fn box_arms(ch: char) -> Option<[u8; 4]> {
    Some(match ch {
        '─' => [1, 1, 0, 0],
        '━' => [2, 2, 0, 0],
        '│' => [0, 0, 1, 1],
        '┃' => [0, 0, 2, 2],
        '┌' => [0, 1, 0, 1],
        '┐' => [1, 0, 0, 1],
        '└' => [0, 1, 1, 0],
        '┘' => [1, 0, 1, 0],
        '┏' => [0, 2, 0, 2],
        '┓' => [2, 0, 0, 2],
        '┗' => [0, 2, 2, 0],
        '┛' => [2, 0, 2, 0],
        '├' => [0, 1, 1, 1],
        '┤' => [1, 0, 1, 1],
        '┬' => [1, 1, 0, 1],
        '┴' => [1, 1, 1, 0],
        '┼' => [1, 1, 1, 1],
        '┣' => [0, 2, 2, 2],
        '┫' => [2, 0, 2, 2],
        '┳' => [2, 2, 0, 2],
        '┻' => [2, 2, 2, 0],
        '╋' => [2, 2, 2, 2],
        '╴' => [1, 0, 0, 0],
        '╵' => [0, 0, 1, 0],
        '╶' => [0, 1, 0, 0],
        '╷' => [0, 0, 0, 1],
        '╸' => [2, 0, 0, 0],
        '╹' => [0, 0, 2, 0],
        '╺' => [0, 2, 0, 0],
        '╻' => [0, 0, 0, 2],
        _ => return None,
    })
}

fn is_shape_char(ch: char) -> bool {
    box_arms(ch).is_some()
        || matches!(ch, '╭'..='╰')
        || (matches!(ch, '\u{2580}'..='\u{259F}') && !matches!(ch, '░'..='▓'))
}

/// Snaps logical lengths to whole device pixels, so lines and block edges
/// land on pixel boundaries and neighbouring cells meet without seams.
#[derive(Clone, Copy)]
struct Snap(f32);

impl Snap {
    fn at(self, v: f32) -> f32 {
        (v * self.0).round() / self.0
    }
}

/// The shapes for `ch` in a `w` × `h` cell at the origin, or `None` when the
/// font draws it.
fn cell_shapes(ch: char, w: f32, h: f32, snap: Snap) -> Option<Vec<Shape>> {
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| {
        let (x0, y0, x1, y1) = (snap.at(x0), snap.at(y0), snap.at(x1), snap.at(y1));
        Shape::Rect(Bounds::new(
            point(px(x0), px(y0)),
            size(px(x1 - x0), px(y1 - y0)),
        ))
    };
    if let Some(arms) = box_arms(ch) {
        let weight = |arm: u8| if arm == 2 { HEAVY } else { LIGHT };
        let heaviest = arms
            .iter()
            .copied()
            .filter(|a| *a > 0)
            .map(weight)
            .fold(0., f32::max);
        // The band a line of thickness `t` takes around the centre `c`.
        let band = |c: f32, t: f32| {
            let start = snap.at(c - t / 2.);
            (start, start + t)
        };
        let (cx0, cx1) = band(w / 2., heaviest);
        let (cy0, cy1) = band(h / 2., heaviest);
        let [left, right, up, down] = arms;
        let mut shapes = Vec::new();
        if left > 0 {
            let (y0, y1) = band(h / 2., weight(left));
            shapes.push(rect(0., y0, cx1, y1));
        }
        if right > 0 {
            let (y0, y1) = band(h / 2., weight(right));
            shapes.push(rect(cx0, y0, w, y1));
        }
        if up > 0 {
            let (x0, x1) = band(w / 2., weight(up));
            shapes.push(rect(x0, 0., x1, cy1));
        }
        if down > 0 {
            let (x0, x1) = band(w / 2., weight(down));
            shapes.push(rect(x0, cy0, x1, h));
        }
        return Some(shapes);
    }
    let code = u32::from(ch);
    let (half_w, half_h) = (w / 2., h / 2.);
    let shapes = match ch {
        '╭'..='╰' => {
            // The middle of the light line's band, so the arc meets it.
            let (x0, x1) = band_mid(w / 2., snap);
            let (y0, y1) = band_mid(h / 2., snap);
            let (cx, cy) = ((x0 + x1) / 2., (y0 + y1) / 2.);
            let (vertical_y, horizontal_x) = match ch {
                '╭' => (h, w),
                '╮' => (h, 0.),
                '╯' => (0., 0.),
                _ => (0., w), // ╰
            };
            vec![Shape::Curve {
                from: point(px(cx), px(vertical_y)),
                ctrl: point(px(cx), px(cy)),
                to: point(px(horizontal_x), px(cy)),
            }]
        }
        '▀' => vec![rect(0., 0., w, half_h)],
        '▁'..='█' => {
            let eighths = (code - 0x2580) as f32;
            vec![rect(0., h - h * eighths / 8., w, h)]
        }
        '▉'..='▏' => {
            let eighths = (0x2590 - code) as f32;
            vec![rect(0., 0., w * eighths / 8., h)]
        }
        '▐' => vec![rect(half_w, 0., w, h)],
        '▔' => vec![rect(0., 0., w, h / 8.)],
        '▕' => vec![rect(w - w / 8., 0., w, h)],
        '▖'..='▟' => {
            // Quarters: upper-left, upper-right, lower-left, lower-right.
            let [ul, ur, ll, lr] = match ch {
                '▖' => [false, false, true, false],
                '▗' => [false, false, false, true],
                '▘' => [true, false, false, false],
                '▙' => [true, false, true, true],
                '▚' => [true, false, false, true],
                '▛' => [true, true, true, false],
                '▜' => [true, true, false, true],
                '▝' => [false, true, false, false],
                '▞' => [false, true, true, false],
                _ => [false, true, true, true], // ▟
            };
            let mut shapes = Vec::new();
            if ul {
                shapes.push(rect(0., 0., half_w, half_h));
            }
            if ur {
                shapes.push(rect(half_w, 0., w, half_h));
            }
            if ll {
                shapes.push(rect(0., half_h, half_w, h));
            }
            if lr {
                shapes.push(rect(half_w, half_h, w, h));
            }
            shapes
        }
        _ => return None,
    };
    Some(shapes)
}

/// A light line's band around `c`.
fn band_mid(c: f32, snap: Snap) -> (f32, f32) {
    let start = snap.at(c - LIGHT / 2.);
    (start, start + LIGHT)
}

// ----- rows -----

/// A cached, laid-out row, positions relative to the row's top-left.
pub(crate) struct RowLayout {
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    runs: Vec<(Pixels, ShapedLine)>,
    shapes: Vec<(Shape, Hsla)>,
    overlines: Vec<(Bounds<Pixels>, Hsla)>,
}

impl RowLayout {
    fn build(
        cells: &[Cell],
        frame: &Frame,
        font: &FontConfig,
        metrics: CellMetrics,
        scale: f32,
        window: &mut Window,
    ) -> Self {
        let cw = metrics.cell_width;
        let lh = metrics.line_height;
        let column = |col: u16| cw * f32::from(col);
        let resolved: Vec<(Rgb, Option<Rgb>)> = cells
            .iter()
            .map(|cell| resolve_colors(cell, frame.foreground, frame.background))
            .collect();

        let backgrounds = background_spans(resolved.iter().map(|(_, bg)| *bg))
            .into_iter()
            .map(|(col, len, color)| {
                let bounds = Bounds::new(point(column(col), px(0.)), size(cw * f32::from(len), lh));
                (bounds, theme::to_hsla(color))
            })
            .collect();

        let mut shapes = Vec::new();
        let mut overlines = Vec::new();
        let snap = Snap(scale.max(1.));
        for (col, (cell, (fg, _))) in cells.iter().zip(&resolved).enumerate() {
            let x = column(col as u16);
            if cell.attrs.overline {
                overlines.push((
                    Bounds::new(point(x, px(0.)), size(cw, px(1.))),
                    theme::to_hsla(*fg),
                ));
            }
            if is_shape_cell(cell)
                && let Some(cell_shapes) = cell_shapes(cell.ch, f32::from(cw), f32::from(lh), snap)
            {
                let color = theme::to_hsla(*fg);
                shapes.extend(
                    cell_shapes
                        .into_iter()
                        .map(|shape| (shape.offset(point(x, px(0.))), color)),
                );
            }
        }

        let text_system = window.text_system().clone();
        let runs = text_pieces(cells, frame.foreground, frame.background)
            .into_iter()
            .map(|piece| match piece {
                Piece::Run {
                    col,
                    cells,
                    text,
                    style,
                } => {
                    let run = style.text_run(font, text.len(), style.fg);
                    let mut line = text_system.shape_line(text.into(), font.size, &[run], Some(cw));
                    // Decorations run to the layout's width: make it the
                    // run's columns, not the font's own advances.
                    force_line_width(&mut line, cw * f32::from(cells));
                    (column(col), line)
                }
                Piece::Wide { col, text, style } => {
                    let run = style.text_run(font, text.len(), style.fg);
                    let text: SharedString = text.into();
                    let mut line = text_system.shape_line(
                        text.clone(),
                        font.size,
                        std::slice::from_ref(&run),
                        None,
                    );
                    let room = cw * 2.;
                    if line.width() > room {
                        let fitted = font.size * (room / line.width());
                        line = text_system.shape_line(text, fitted, &[run], None);
                    }
                    (column(col), line)
                }
            })
            .collect();

        Self {
            backgrounds,
            runs,
            shapes,
            overlines,
        }
    }
}

/// Give a shaped line the width `width`, keeping its glyphs.
fn force_line_width(line: &mut ShapedLine, width: Pixels) {
    let layout: &mut Arc<gpui_kit::LineLayout> = line;
    *layout = Arc::new(gpui_kit::LineLayout {
        font_size: layout.font_size,
        width,
        ascent: layout.ascent,
        descent: layout.descent,
        runs: layout.runs.clone(),
        len: layout.len,
    });
}

// ----- cursor glyph cache -----

/// The character under a block cursor, shaped once per change.
#[derive(Default)]
pub(crate) struct CursorGlyph {
    key: Option<(String, RunStyle, Rgb, FontConfig)>,
    line: Option<ShapedLine>,
}

impl CursorGlyph {
    fn get(
        &mut self,
        text: String,
        style: RunStyle,
        color: Rgb,
        font: &FontConfig,
        window: &mut Window,
    ) -> ShapedLine {
        let key = (text, style, color, font.clone());
        if self.key.as_ref() != Some(&key) || self.line.is_none() {
            let (text, style, color, font) = &key;
            let run = style.text_run(font, text.len(), *color);
            self.line = Some(window.text_system().shape_line(
                text.clone().into(),
                font.size,
                &[run],
                None,
            ));
            self.key = Some(key);
        }
        self.line.clone().unwrap_or_default()
    }
}

// ----- the element -----

pub(crate) struct TerminalElement {
    view: Entity<TerminalView>,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

enum CursorPaint {
    Fill(Bounds<Pixels>, Hsla),
    Outline(Bounds<Pixels>, Hsla),
}

pub(crate) struct Prepaint {
    hitbox: Hitbox,
    grid: Grid,
    background: Hsla,
    rows: Vec<Option<Rc<RowLayout>>>,
    selections: Vec<Bounds<Pixels>>,
    selection_color: Hsla,
    cursor: Option<CursorPaint>,
    cursor_glyph: Option<(Point<Pixels>, ShapedLine)>,
    ime: Option<(Bounds<Pixels>, ShapedLine)>,
    knob: Option<(Bounds<Pixels>, Hsla)>,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.;
        style.flex_shrink = 1.;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let selection_color = theme::selection(cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.view.update(cx, |view, cx| {
            view.sync_layout(bounds, window, cx);
            let grid = view.grid.expect("sync_layout stores the grid");
            let m = grid.metrics;

            // Rebuild the rows that changed; keep the rest as they are.
            for (row, slot) in view.rows_cache.iter_mut().enumerate() {
                if slot.is_none() {
                    let cells = view
                        .frame
                        .lines
                        .get(row)
                        .map(|line| line.cells.as_slice())
                        .unwrap_or_default();
                    *slot = Some(Rc::new(RowLayout::build(
                        cells,
                        &view.frame,
                        &view.font,
                        m,
                        grid.scale,
                        window,
                    )));
                }
            }

            let cell = |col: u16, row: u16, cols: u16| {
                Bounds::new(
                    grid.origin
                        + point(
                            m.cell_width * f32::from(col),
                            m.line_height * f32::from(row),
                        ),
                    size(m.cell_width * f32::from(cols), m.line_height),
                )
            };
            let frame = &view.frame;
            let selections = view
                .selection_rows()
                .map(|(row, start, end)| cell(start, row, end.saturating_sub(start) + 1))
                .collect();

            let mut cursor = None;
            let mut cursor_glyph = None;
            let mut ime = None;
            let fg = theme::to_hsla(frame.foreground);
            if let Some((col, row)) = frame.cursor.position {
                if let Some(text) = view.marked_text.clone() {
                    let run = TextRun {
                        len: text.len(),
                        font: view.font.font(false, false),
                        color: fg,
                        background_color: None,
                        underline: underline_style(Underline::Single, fg),
                        strikethrough: None,
                    };
                    let line =
                        window
                            .text_system()
                            .shape_line(text.into(), view.font.size, &[run], None);
                    let patch = cell(col, row, 1);
                    let width = line.width().max(m.cell_width);
                    ime = Some((Bounds::new(patch.origin, size(width, m.line_height)), line));
                } else if frame.cursor.visible && (!view.focused || view.blink_visible) {
                    let rect = cell(col, row, if frame.cursor.wide { 2 } else { 1 });
                    let color = theme::to_hsla(frame.cursor_color.unwrap_or(frame.foreground));
                    let thin = px(2.).min(m.cell_width / 3.);
                    cursor = Some(match (view.focused, frame.cursor.shape) {
                        (false, _) | (true, CursorShape::BlockHollow) => {
                            CursorPaint::Outline(rect, color)
                        }
                        (true, CursorShape::Bar) => CursorPaint::Fill(
                            Bounds::new(rect.origin, size(thin, rect.size.height)),
                            color,
                        ),
                        (true, CursorShape::Underline) => CursorPaint::Fill(
                            Bounds::new(
                                point(rect.origin.x, rect.bottom() - thin),
                                size(rect.size.width, thin),
                            ),
                            color,
                        ),
                        (true, CursorShape::Block) => CursorPaint::Fill(rect, color),
                    });
                    if view.focused && frame.cursor.shape == CursorShape::Block {
                        let under = frame
                            .lines
                            .get(usize::from(row))
                            .and_then(|line| line.cells.get(usize::from(col)))
                            .filter(|c| c.has_text() && !c.attrs.invisible)
                            .cloned();
                        if let Some(under) = under {
                            let (_, bg) =
                                resolve_colors(&under, frame.foreground, frame.background);
                            let color = bg.unwrap_or(frame.background);
                            let style = RunStyle {
                                bold: under.attrs.bold,
                                italic: under.attrs.italic,
                                fg: color,
                                underline: under.attrs.underline,
                                underline_color: under.underline_color.unwrap_or(color),
                                strikethrough: under.attrs.strikethrough,
                            };
                            let font = view.font.clone();
                            let line =
                                view.cursor_glyph
                                    .get(under.text(), style, color, &font, window);
                            cursor_glyph = Some((rect.origin, line));
                        }
                    }
                }
            }

            let knob = view.scrollbar().filter(|s| s.total > s.len).map(|s| {
                let track = grid.bounds;
                let height =
                    (track.size.height * (s.len as f32 / s.total as f32)).max(KNOB_MIN_HEIGHT);
                let travel = (track.size.height - height).max(px(0.));
                let top = travel * (s.offset as f32 / s.total.saturating_sub(s.len).max(1) as f32);
                let mut color = fg;
                color.a = KNOB_ALPHA;
                (
                    Bounds::new(
                        point(track.right() - KNOB_INSET, track.top() + top),
                        size(KNOB_WIDTH, height),
                    ),
                    color,
                )
            });

            Prepaint {
                hitbox,
                grid,
                background: theme::to_hsla(frame.background),
                rows: view.rows_cache.clone(),
                selections,
                selection_color,
                cursor,
                cursor_glyph,
                ime,
                knob,
            }
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.register_input(bounds, &prepaint.hitbox, window, cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint_content(prepaint, window, cx);
        });
    }
}

impl TerminalElement {
    fn register_input(
        &self,
        bounds: Bounds<Pixels>,
        hitbox: &Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let focus = self.view.read(cx).focus_handle(cx);
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let (view, hitbox_down) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox_down.is_hovered(window) {
                return;
            }
            view.update(cx, |view, cx| view.mouse_down(event, window, cx));
            if view.read(cx).keeps_press(event) {
                cx.stop_propagation();
            }
        });
        let (view, hitbox_move) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let hovered = hitbox_move.is_hovered(window);
            view.update(cx, |view, cx| view.mouse_move(event, hovered, cx));
        });
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |view, cx| view.mouse_up(event, cx));
            }
        });
        let (view, hitbox_scroll) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox_scroll.should_handle_scroll(window) {
                view.update(cx, |view, cx| view.scroll_wheel(event, cx));
                cx.stop_propagation();
            }
        });
    }
}

fn paint_content(p: &mut Prepaint, window: &mut Window, cx: &mut App) {
    let grid = p.grid;
    let lh = grid.metrics.line_height;
    let row_origin = |row: usize| grid.origin + point(px(0.), lh * row as f32);
    let at = |bounds: &Bounds<Pixels>, origin: Point<Pixels>| {
        Bounds::new(bounds.origin + origin, bounds.size)
    };

    window.paint_quad(fill(grid.bounds, p.background));
    for (row, layout) in p.rows.iter().enumerate() {
        let Some(layout) = layout else { continue };
        let origin = row_origin(row);
        for (bounds, color) in &layout.backgrounds {
            window.paint_quad(fill(at(bounds, origin), *color));
        }
    }
    for bounds in &p.selections {
        window.paint_quad(fill(*bounds, p.selection_color));
    }
    for (row, layout) in p.rows.iter().enumerate() {
        let Some(layout) = layout else { continue };
        let origin = row_origin(row);
        for (x, line) in &layout.runs {
            let _ = line.paint(
                origin + point(*x, px(0.)),
                lh,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        for (shape, color) in &layout.shapes {
            paint_shape(&shape.offset(origin), *color, window);
        }
        for (bounds, color) in &layout.overlines {
            window.paint_quad(fill(at(bounds, origin), *color));
        }
    }

    match &p.cursor {
        Some(CursorPaint::Fill(bounds, color)) => window.paint_quad(fill(*bounds, *color)),
        Some(CursorPaint::Outline(bounds, color)) => {
            window.paint_quad(outline(*bounds, *color, BorderStyle::Solid))
        }
        None => {}
    }
    if let Some((origin, line)) = &p.cursor_glyph {
        let _ = line.paint(*origin, lh, TextAlign::Left, None, window, cx);
    }

    if let Some((bounds, line)) = &p.ime {
        window.paint_quad(fill(*bounds, p.background));
        let _ = line.paint(bounds.origin, lh, TextAlign::Left, None, window, cx);
    }

    if let Some((bounds, color)) = p.knob {
        window.paint_quad(fill(bounds, color).corner_radii(KNOB_RADIUS));
    }
}

fn paint_shape(shape: &Shape, color: Hsla, window: &mut Window) {
    match shape {
        Shape::Rect(bounds) => window.paint_quad(fill(*bounds, color)),
        Shape::Curve { from, ctrl, to } => {
            let mut path = PathBuilder::stroke(px(LIGHT));
            path.move_to(*from);
            path.curve_to(*to, *ctrl);
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Attrs;

    const FG: Rgb = Rgb::new(200, 200, 200);
    const BG: Rgb = Rgb::new(10, 10, 10);

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            ..Cell::default()
        }
    }

    fn row(text: &str) -> Vec<Cell> {
        text.chars().map(cell).collect()
    }

    fn texts(pieces: &[Piece]) -> Vec<(u16, String)> {
        pieces
            .iter()
            .map(|p| match p {
                Piece::Run { col, text, .. } | Piece::Wide { col, text, .. } => {
                    (*col, text.clone())
                }
            })
            .collect()
    }

    #[test]
    fn inverse_and_faint_resolve_colours() {
        let red = Rgb::new(255, 0, 0);
        let blue = Rgb::new(0, 0, 255);
        let mut c = cell('x');
        c.attrs.inverse = true;
        assert_eq!(resolve_colors(&c, FG, BG), (BG, Some(FG)));
        c.fg = Some(red);
        c.bg = Some(blue);
        assert_eq!(resolve_colors(&c, FG, BG), (blue, Some(red)));

        let mut faint = cell('x');
        faint.attrs.faint = true;
        assert_eq!(resolve_colors(&faint, FG, BG), (FG.blend(BG, 0.45), None));
        faint.attrs.inverse = true;
        // Inverse first, then faint toward the new background (the old fg).
        assert_eq!(
            resolve_colors(&faint, FG, BG),
            (BG.blend(FG, 0.45), Some(FG))
        );
    }

    #[test]
    fn background_spans_merge() {
        let (ra, rb) = (Rgb::new(1, 2, 3), Rgb::new(4, 5, 6));
        let (a, b) = (Some(ra), Some(rb));
        assert_eq!(
            background_spans([a, a, a, None, a, b, b]),
            vec![(0, 3, ra), (4, 1, ra), (5, 2, rb)]
        );
        assert!(background_spans([None, None]).is_empty());
    }

    #[test]
    fn runs_batch_by_style() {
        // Words and the spaces between them: one shaping call.
        let pieces = text_pieces(&row("foo bar   "), FG, BG);
        assert_eq!(texts(&pieces), vec![(0, "foo bar".into())]);
        match &pieces[0] {
            Piece::Run { cells, .. } => assert_eq!(*cells, 7),
            other => panic!("{other:?}"),
        }

        // A colour change splits.
        let mut cells = row("ab");
        cells[1].fg = Some(Rgb::new(255, 0, 0));
        assert_eq!(
            texts(&text_pieces(&cells, FG, BG)),
            vec![(0, "a".into()), (1, "b".into())]
        );

        // A wide character is alone and splits the run around it.
        let mut cells = row("a漢 b");
        cells[1].width = CellWidth::Wide;
        cells[2] = Cell {
            width: CellWidth::SpacerTail,
            ..Cell::default()
        };
        let pieces = text_pieces(&cells, FG, BG);
        assert_eq!(
            texts(&pieces),
            vec![(0, "a".into()), (1, "漢".into()), (3, "b".into())]
        );
        assert!(matches!(pieces[1], Piece::Wide { .. }));

        // An underlined space starts a run, and stays when it ends one.
        let mut cells = row("a  ");
        cells[1].attrs.underline = Underline::Single;
        cells[2].attrs.underline = Underline::Single;
        assert_eq!(
            texts(&text_pieces(&cells, FG, BG)),
            vec![(0, "a".into()), (1, "  ".into())]
        );

        // A blank never starts a run; leading and trailing blanks go.
        assert_eq!(
            texts(&text_pieces(&row("  x  "), FG, BG)),
            vec![(2, "x".into())]
        );

        // Box drawing ends the run before it.
        assert_eq!(
            texts(&text_pieces(&row("ab─cd"), FG, BG)),
            vec![(0, "ab".into()), (3, "cd".into())]
        );

        // Invisible text draws nothing.
        let mut cells = row("ab");
        cells[0].attrs = Attrs {
            invisible: true,
            ..Attrs::default()
        };
        assert_eq!(texts(&text_pieces(&cells, FG, BG)), vec![(1, "b".into())]);
    }

    fn rects(ch: char, w: f32, h: f32) -> Vec<Bounds<Pixels>> {
        cell_shapes(ch, w, h, Snap(1.))
            .unwrap()
            .into_iter()
            .map(|s| match s {
                Shape::Rect(b) => b,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn b(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn box_geometry() {
        // ┼: four 1 px arms from the edges, overlapping at the centre.
        let cross = rects('┼', 8., 16.);
        // At scale 1 the 1 px lines land on whole pixels next to the centre.
        assert_eq!(
            cross,
            vec![
                b(0., 8., 5., 1.),
                b(4., 8., 4., 1.),
                b(4., 0., 1., 9.),
                b(4., 8., 1., 8.),
            ]
        );
        // ━: 2 px arms, edge to edge.
        let heavy = rects('━', 8., 16.);
        assert!(heavy.iter().all(|r| r.size.height == px(2.)));
        assert_eq!(heavy[0].left(), px(0.));
        assert_eq!(heavy[1].right(), px(8.));
        // ╭: from the bottom middle to the right middle, bending at the centre.
        match &cell_shapes('╭', 8., 16., Snap(1.)).unwrap()[..] {
            [Shape::Curve { from, ctrl, to }] => {
                // Along the middle of the straight lines' 1 px bands.
                assert_eq!(*from, point(px(4.5), px(16.)));
                assert_eq!(*ctrl, point(px(4.5), px(8.5)));
                assert_eq!(*to, point(px(8.), px(8.5)));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(rects('▄', 8., 16.), vec![b(0., 8., 8., 8.)]);
        assert_eq!(
            rects('▚', 8., 16.),
            vec![b(0., 0., 4., 8.), b(4., 8., 4., 8.)]
        );
        assert_eq!(rects('▏', 8., 16.), vec![b(0., 0., 1., 16.)]);
        assert_eq!(rects('█', 8., 16.), vec![b(0., 0., 8., 16.)]);
        assert!(cell_shapes('═', 8., 16., Snap(1.)).is_none());
        assert!(cell_shapes('░', 8., 16., Snap(1.)).is_none());
        assert!(!is_shape_char('═') && !is_shape_char('▒') && is_shape_char('▟'));
    }

    #[test]
    fn metric_fallbacks_and_device_rounding() {
        // No advance: 0.6 × size. No extent: 1.2 × size.
        let m = CellMetrics::from_font(None, px(0.), px(0.), px(10.), 1.);
        assert_eq!(
            m,
            CellMetrics {
                cell_width: px(6.),
                line_height: px(12.)
            }
        );
        let m = CellMetrics::from_font(Some(px(0.5)), px(0.), px(0.), px(10.), 1.);
        assert_eq!(m.cell_width, px(6.));
        // Width rounds, height rounds up, in device pixels.
        let m = CellMetrics::from_font(Some(px(7.8)), px(12.1), px(-3.), px(13.), 2.);
        assert_eq!(
            m,
            CellMetrics {
                cell_width: px(8.),
                line_height: px(15.5)
            }
        );
        assert_eq!(
            (m.device_cell_width(2.), m.device_line_height(2.)),
            (16, 31)
        );
        let m = CellMetrics::from_font(Some(px(7.8)), px(12.1), px(3.), px(13.), 1.5);
        assert_eq!(m.cell_width, px(8.));
        assert_eq!(m.line_height, px(23. / 1.5));
        assert_eq!(
            (m.device_cell_width(1.5), m.device_line_height(1.5)),
            (12, 23)
        );
        // Below 1 the scale counts as 1.
        assert_eq!(m.device_cell_width(0.5), 8);
    }
}
