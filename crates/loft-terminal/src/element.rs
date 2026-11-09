//! The GPUI element that paints a [`TerminalView`]'s grid and routes pointer
//! input back to it.
//!
//! Rows are shaped once and cached ([`RowLayout`]); the cache entry for a row
//! is dropped only when the engine reports that row as damaged, so steady
//! state frames re-shape nothing. Within a row, cells with the same style
//! are shaped as one run with `force_width = cell width`, which snaps each
//! glyph to its column. Wide characters run alone, positioned by column.
//! Box-drawing lines and block elements are drawn as geometry so they join
//! seamlessly across cells and rows.
//!
//! Structure adapted from Ghostex's `terminal_element.rs` (MIT, see
//! THIRD_PARTY_NOTICES.md).

use crate::backend::{Cell, CellWidth, CursorShape, Frame, Line, Rgb, Underline};
use crate::theme::{self, to_hsla};
use crate::view::TerminalView;
use gpui_kit::{
    App, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId, ElementInputHandler,
    Entity, Font, FontFeatures, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior,
    Hsla, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point, ScrollEvent, ShapedLine, SharedString, Size, StrikethroughStyle,
    Style, TextAlign, TextRun, UnderlineStyle, Window, fill, outline, point, px, size,
};
use std::rc::Rc;

/// Inner padding between the element edge and the grid.
pub(crate) const PADDING: Size<Pixels> = Size {
    width: px(8.),
    height: px(6.),
};

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
    fn font(&self, bold: bool, italic: bool) -> Font {
        let mut font = gpui_kit::font(self.family.clone());
        // Ligatures would merge cells; a terminal grid needs one glyph per
        // cell.
        font.features = FontFeatures::disable_ligatures();
        font.weight = if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        font.style = if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        font
    }
}

/// Size of one cell, snapped to whole device pixels so cell edges are crisp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellMetrics {
    pub cell_width: Pixels,
    pub line_height: Pixels,
}

impl CellMetrics {
    pub fn measure(font: &FontConfig, window: &mut Window) -> Self {
        let scale = window.scale_factor().max(1.);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font.font(false, false));
        let advance = text_system
            .advance(font_id, font.size, 'm')
            .map(|s| s.width)
            .unwrap_or(font.size * 0.6);
        let ascent = text_system.ascent(font_id, font.size);
        let descent = text_system.descent(font_id, font.size);
        // Fall back to typical monospace proportions if the font reports
        // nothing usable (a missing font, or a headless text system).
        let size = f32::from(font.size);
        let advance = Some(f32::from(advance))
            .filter(|a| *a >= 1.)
            .unwrap_or(size * 0.6);
        let height = Some(f32::from(ascent) + f32::from(descent).abs())
            .filter(|h| *h >= 1.)
            .unwrap_or(size * 1.2);
        Self {
            cell_width: px((advance * scale).round().max(1.) / scale),
            line_height: px((height * scale).ceil().max(1.) / scale),
        }
    }

    pub fn device_cell_width(&self, scale: f32) -> u32 {
        ((f32::from(self.cell_width) * scale).round() as u32).max(1)
    }

    pub fn device_line_height(&self, scale: f32) -> u32 {
        ((f32::from(self.line_height) * scale).round() as u32).max(1)
    }
}

/// Geometry drawn instead of a glyph, relative to the row origin.
#[derive(Clone, Debug)]
enum Shape {
    Rect(Bounds<Pixels>, Hsla),
    /// A rounded box corner: quarter arc from the edge midpoint `from`
    /// through the cell center to the edge midpoint `to`.
    Arc {
        from: Point<Pixels>,
        center: Point<Pixels>,
        to: Point<Pixels>,
        width: Pixels,
        color: Hsla,
    },
}

/// A cached, shaped row.
pub(crate) struct RowLayout {
    /// Merged background spans: (column, length, color).
    backgrounds: Vec<(u16, u16, Hsla)>,
    /// Shaped text runs positioned by column.
    runs: Vec<(u16, ShapedLine)>,
    shapes: Vec<Shape>,
}

struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    focused: bool,
    color: Hsla,
    /// The glyph under a solid block cursor, redrawn in the background color.
    glyph: Option<ShapedLine>,
}

struct MarkedLayout {
    origin: Point<Pixels>,
    background: Bounds<Pixels>,
    text: ShapedLine,
}

/// Everything `paint` needs, built in `prepaint`.
pub(crate) struct FrameLayout {
    origin: Point<Pixels>,
    metrics: CellMetrics,
    background: Hsla,
    rows: Vec<Rc<RowLayout>>,
    selections: Vec<(u16, u16, u16)>,
    selection_color: Hsla,
    cursor: Option<CursorLayout>,
    marked: Option<MarkedLayout>,
    scrollbar: Option<(Bounds<Pixels>, Hsla)>,
}

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

pub(crate) struct PrepaintState {
    layout: FrameLayout,
    hitbox: Hitbox,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

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
        let style = Style {
            size: Size::full(),
            flex_grow: 1.,
            flex_shrink: 1.,
            ..Style::default()
        };
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
        let layout = self.view.update(cx, |view, cx| {
            view.sync_layout(bounds, window, cx);
            build_frame_layout(view, selection_color, window)
        });
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        PrepaintState { layout, hitbox }
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
        register_input(&self.view, bounds, &prepaint.hitbox, window, cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint_frame(bounds, &prepaint.layout, window, cx);
        });
    }
}

fn register_input(
    view: &Entity<TerminalView>,
    bounds: Bounds<Pixels>,
    hitbox: &Hitbox,
    window: &mut Window,
    cx: &mut App,
) {
    window.set_cursor_style(CursorStyle::IBeam, hitbox);
    let focus_handle = view.read(cx).focus_handle(cx);
    window.handle_input(
        &focus_handle,
        ElementInputHandler::new(bounds, view.clone()),
        cx,
    );

    window.on_mouse_event({
        let view = view.clone();
        let hitbox = hitbox.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                view.update(cx, |view, cx| view.mouse_down(event, window, cx));
                cx.stop_propagation();
            }
        }
    });
    // Drags leave the element, so move/up listen window-wide; the view
    // gates on its own gesture state.
    window.on_mouse_event({
        let view = view.clone();
        let hitbox = hitbox.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hitbox.is_hovered(window);
                view.update(cx, |view, cx| view.mouse_move(event, hovered, cx));
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseUpEvent, phase, _window, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |view, cx| view.mouse_up(event, cx));
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        let hitbox = hitbox.clone();
        move |event: &ScrollEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                view.update(cx, |view, cx| view.scroll_wheel(event, cx));
                cx.stop_propagation();
            }
        }
    });
}

use gpui_kit::Focusable as _;

fn build_frame_layout(
    view: &mut TerminalView,
    selection_color: Hsla,
    window: &mut Window,
) -> FrameLayout {
    let grid = view
        .grid
        .expect("sync_layout always sets the grid before frame layout");
    let metrics = grid.metrics;
    let frame = &view.frame;
    let empty = Line::default();
    for (row, slot) in view.rows_cache.iter_mut().enumerate() {
        if slot.is_none() {
            let line = frame.lines.get(row).unwrap_or(&empty);
            *slot = Some(Rc::new(build_row(line, frame, &view.font, metrics, window)));
        }
    }
    let rows = view.rows_cache.iter().flatten().cloned().collect();
    let foreground = to_hsla(frame.foreground);
    let background = to_hsla(frame.background);

    let cursor = frame.cursor.position.and_then(|(col, row)| {
        if !frame.cursor.visible || view.marked_text.is_some() {
            return None;
        }
        if view.focused && !view.blink_visible {
            return None;
        }
        let width = if frame.cursor.wide { 2. } else { 1. };
        let bounds = Bounds::new(
            grid.origin
                + point(
                    metrics.cell_width * f32::from(col),
                    metrics.line_height * f32::from(row),
                ),
            size(metrics.cell_width * width, metrics.line_height),
        );
        let color = to_hsla(frame.cursor_color.unwrap_or(frame.foreground));
        let solid = view.focused && frame.cursor.shape == CursorShape::Block;
        let glyph = solid
            .then(|| {
                let cell = frame
                    .lines
                    .get(usize::from(row))?
                    .cells
                    .get(usize::from(col))?;
                if !cell.has_text() || cell.attrs.invisible {
                    return None;
                }
                let text = cell.text();
                let (_, bg) = resolve_colors(cell, frame);
                let mut run = text_run(cell, bg.unwrap_or(frame.background), &view.font);
                run.len = text.len();
                Some(
                    window
                        .text_system()
                        .shape_line(text.into(), view.font.size, &[run], None),
                )
            })
            .flatten();
        Some(CursorLayout {
            bounds,
            shape: frame.cursor.shape,
            focused: view.focused,
            color,
            glyph,
        })
    });

    let marked = view.marked_text.as_ref().and_then(|text| {
        let (col, row) = frame.cursor.position?;
        let origin = grid.origin
            + point(
                metrics.cell_width * f32::from(col),
                metrics.line_height * f32::from(row),
            );
        let run = TextRun {
            len: text.len(),
            font: view.font.font(false, false),
            color: foreground,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(foreground),
                wavy: false,
            }),
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(text.clone()),
            view.font.size,
            &[run],
            None,
        );
        let width = shaped.width.max(metrics.cell_width);
        Some(MarkedLayout {
            origin,
            background: Bounds::new(origin, size(width, metrics.line_height)),
            text: shaped,
        })
    });

    let scrollbar = view.scrollbar().filter(|s| s.total > s.len).map(|bar| {
        let track = grid.bounds;
        let height = track.size.height;
        let knob_h = (height * (bar.len as f32 / bar.total as f32)).max(px(24.));
        let max_offset = (bar.total - bar.len).max(1) as f32;
        let top = (height - knob_h) * (bar.offset as f32 / max_offset);
        let mut color = foreground;
        color.a = 0.35;
        (
            Bounds::new(
                track.origin + point(track.size.width - px(7.), top),
                size(px(4.), knob_h),
            ),
            color,
        )
    });

    FrameLayout {
        origin: grid.origin,
        metrics,
        background,
        rows,
        selections: view.selection_rows().collect(),
        selection_color,
        cursor,
        marked,
        scrollbar,
    }
}

fn paint_frame(bounds: Bounds<Pixels>, layout: &FrameLayout, window: &mut Window, cx: &mut App) {
    let m = layout.metrics;
    window.paint_quad(fill(bounds, layout.background));

    for (row, row_layout) in layout.rows.iter().enumerate() {
        let y = layout.origin.y + m.line_height * row as f32;
        for &(col, len, color) in &row_layout.backgrounds {
            window.paint_quad(fill(
                Bounds::new(
                    point(layout.origin.x + m.cell_width * f32::from(col), y),
                    size(m.cell_width * f32::from(len), m.line_height),
                ),
                color,
            ));
        }
    }

    for &(row, start, end) in &layout.selections {
        let y = layout.origin.y + m.line_height * f32::from(row);
        window.paint_quad(fill(
            Bounds::new(
                point(layout.origin.x + m.cell_width * f32::from(start), y),
                size(
                    m.cell_width * f32::from(end.saturating_sub(start) + 1),
                    m.line_height,
                ),
            ),
            layout.selection_color,
        ));
    }

    for (row, row_layout) in layout.rows.iter().enumerate() {
        let row_origin = point(
            layout.origin.x,
            layout.origin.y + m.line_height * row as f32,
        );
        for (col, shaped) in &row_layout.runs {
            let origin = row_origin + point(m.cell_width * f32::from(*col), px(0.));
            let _ = shaped.paint(origin, m.line_height, TextAlign::Left, None, window, cx);
        }
        for shape in &row_layout.shapes {
            paint_shape(row_origin, shape, window);
        }
    }

    if let Some(cursor) = &layout.cursor {
        paint_cursor(cursor, m, window, cx);
    }

    if let Some(marked) = &layout.marked {
        window.paint_quad(fill(marked.background, layout.background));
        let _ = marked.text.paint(
            marked.origin,
            m.line_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    if let Some((knob, color)) = layout.scrollbar {
        window.paint_quad(fill(knob, color).corner_radii(px(2.)));
    }
}

fn paint_cursor(cursor: &CursorLayout, m: CellMetrics, window: &mut Window, cx: &mut App) {
    let b = cursor.bounds;
    let thickness = px(2.).min(m.cell_width / 3.);
    match (cursor.focused, cursor.shape) {
        (true, CursorShape::Block) => {
            window.paint_quad(fill(b, cursor.color));
            if let Some(glyph) = &cursor.glyph {
                let _ = glyph.paint(b.origin, m.line_height, TextAlign::Left, None, window, cx);
            }
        }
        (true, CursorShape::Bar) => {
            window.paint_quad(fill(
                Bounds::new(b.origin, size(thickness, b.size.height)),
                cursor.color,
            ));
        }
        (true, CursorShape::Underline) => {
            window.paint_quad(fill(
                Bounds::new(
                    point(b.origin.x, b.origin.y + b.size.height - thickness),
                    size(b.size.width, thickness),
                ),
                cursor.color,
            ));
        }
        // Hollow block when unfocused, or when the program asks for it.
        _ => {
            window.paint_quad(outline(b, cursor.color, gpui_kit::BorderStyle::Solid));
        }
    }
}

fn paint_shape(row_origin: Point<Pixels>, shape: &Shape, window: &mut Window) {
    match shape {
        Shape::Rect(bounds, color) => {
            window.paint_quad(fill(
                Bounds::new(row_origin + bounds.origin, bounds.size),
                *color,
            ));
        }
        Shape::Arc {
            from,
            center,
            to,
            width,
            color,
        } => {
            let mut path = PathBuilder::stroke(*width);
            path.move_to(row_origin + *from);
            path.curve_to(row_origin + *to, row_origin + *center);
            if let Ok(path) = path.build() {
                window.paint_path(path, *color);
            }
        }
    }
}

/// Foreground and (explicit) background after inverse and faint.
fn resolve_colors(cell: &Cell, frame: &Frame) -> (Rgb, Option<Rgb>) {
    let mut fg = cell.fg.unwrap_or(frame.foreground);
    let mut bg = cell.bg;
    if cell.attrs.inverse {
        let inverted = fg;
        fg = bg.unwrap_or(frame.background);
        bg = Some(inverted);
    }
    if cell.attrs.faint {
        fg = fg.blend(bg.unwrap_or(frame.background), 0.45);
    }
    (fg, bg)
}

fn text_run(cell: &Cell, fg: Rgb, font: &FontConfig) -> TextRun {
    let color = to_hsla(fg);
    let underline = match cell.attrs.underline {
        Underline::None => None,
        kind => Some(UnderlineStyle {
            thickness: if kind == Underline::Double {
                px(2.)
            } else {
                px(1.)
            },
            color: Some(cell.underline_color.map(to_hsla).unwrap_or(color)),
            wavy: kind == Underline::Curly,
        }),
    };
    TextRun {
        len: 0,
        font: font.font(cell.attrs.bold, cell.attrs.italic),
        color,
        background_color: None,
        underline,
        strikethrough: cell.attrs.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    }
}

fn push_span(spans: &mut Vec<(u16, u16, Hsla)>, col: u16, color: Hsla) {
    if let Some(last) = spans.last_mut()
        && last.2 == color
        && last.0 + last.1 == col
    {
        last.1 += 1;
        return;
    }
    spans.push((col, 1, color));
}

/// A run of same-styled cells being collected for one shaping call.
struct Batch {
    col: u16,
    cells: u16,
    text: String,
    run: TextRun,
}

impl Batch {
    fn accepts(&self, run: &TextRun, col: u16) -> bool {
        self.col + self.cells == col
            && self.run.font == run.font
            && self.run.color == run.color
            && self.run.underline == run.underline
            && self.run.strikethrough == run.strikethrough
    }

    /// Blank cells can ride along in a batch without decorations.
    fn accepts_blank(&self, col: u16) -> bool {
        self.col + self.cells == col
            && self.run.underline.is_none()
            && self.run.strikethrough.is_none()
    }
}

fn build_row(
    line: &Line,
    frame: &Frame,
    font: &FontConfig,
    m: CellMetrics,
    window: &mut Window,
) -> RowLayout {
    let mut backgrounds = Vec::new();
    let mut runs = Vec::new();
    let mut shapes = Vec::new();
    let mut batch: Option<Batch> = None;

    let flush =
        |batch: &mut Option<Batch>, runs: &mut Vec<(u16, ShapedLine)>, window: &mut Window| {
            let Some(mut b) = batch.take() else { return };
            b.text.truncate(b.text.trim_end_matches(' ').len());
            if b.text.is_empty() {
                return;
            }
            b.run.len = b.text.len();
            let shaped = window.text_system().shape_line(
                SharedString::from(b.text),
                font.size,
                std::slice::from_ref(&b.run),
                Some(m.cell_width),
            );
            runs.push((b.col, shaped));
        };

    for (col, cell) in line.cells.iter().enumerate() {
        let col = col as u16;
        let (fg, bg) = resolve_colors(cell, frame);
        // Spacer cells carry the wide character's colors, so a wide cell's
        // background covers both columns.
        if let Some(bg) = bg {
            push_span(&mut backgrounds, col, to_hsla(bg));
        }
        if cell.attrs.overline {
            shapes.push(Shape::Rect(
                Bounds::new(
                    point(m.cell_width * f32::from(col), px(0.)),
                    size(m.cell_width, px(1.)),
                ),
                to_hsla(fg),
            ));
        }
        if matches!(cell.width, CellWidth::SpacerTail | CellWidth::SpacerHead) {
            continue;
        }
        let decorated = cell.attrs.underline != Underline::None || cell.attrs.strikethrough;
        if cell.attrs.invisible || (!cell.has_text() && !decorated) {
            if let Some(b) = batch.as_mut()
                && b.accepts_blank(col)
            {
                b.text.push(' ');
                b.cells += 1;
            }
            continue;
        }
        if cell.width == CellWidth::Narrow
            && cell.extra.is_none()
            && !decorated
            && box_drawing(cell.ch, col, to_hsla(fg), m, &mut shapes)
        {
            continue;
        }
        let mut run = text_run(cell, fg, font);
        if cell.width == CellWidth::Wide {
            // A forced single-cell advance would misplace whatever follows a
            // wide glyph, so wide cells are shaped alone.
            flush(&mut batch, &mut runs, window);
            let text: SharedString = cell.text().into();
            run.len = text.len();
            let mut shaped = window.text_system().shape_line(
                text.clone(),
                font.size,
                std::slice::from_ref(&run),
                None,
            );
            // Emoji fonts often advance wider than two cells; shrink the
            // glyph to fit its cells instead of overlapping the neighbor.
            let room = m.cell_width * 2.;
            if shaped.width > room {
                let fitted = font.size * (room / shaped.width);
                shaped = window.text_system().shape_line(text, fitted, &[run], None);
            }
            runs.push((col, shaped));
            continue;
        }
        match batch.as_mut() {
            Some(b) if b.accepts(&run, col) => {
                cell.push_text(&mut b.text);
                b.cells += 1;
            }
            _ => {
                flush(&mut batch, &mut runs, window);
                let mut text = String::new();
                cell.push_text(&mut text);
                batch = Some(Batch {
                    col,
                    cells: 1,
                    text,
                    run,
                });
            }
        }
    }
    flush(&mut batch, &mut runs, window);

    RowLayout {
        backgrounds,
        runs,
        shapes,
    }
}

/// Draw box-drawing lines and block elements as geometry. Returns false for
/// characters left to the font.
fn box_drawing(ch: char, col: u16, color: Hsla, m: CellMetrics, shapes: &mut Vec<Shape>) -> bool {
    let x0 = m.cell_width * f32::from(col);
    let w = m.cell_width;
    let h = m.line_height;
    let mut rect = |x: f32, y: f32, rw: f32, rh: f32| {
        shapes.push(Shape::Rect(
            Bounds::new(point(x0 + w * x, h * y), size(w * rw, h * rh)),
            color,
        ));
    };
    // Block elements (U+2580–U+259F).
    match ch {
        '▀' => {
            rect(0., 0., 1., 0.5);
            return true;
        }
        '▁'..='█' => {
            let eighths = (ch as u32 - 0x2580) as f32 / 8.;
            rect(0., 1. - eighths, 1., eighths);
            return true;
        }
        '▉'..='▏' => {
            rect(0., 0., (0x2590 - ch as u32) as f32 / 8., 1.);
            return true;
        }
        '▐' => {
            rect(0.5, 0., 0.5, 1.);
            return true;
        }
        '▔' => {
            rect(0., 0., 1., 0.125);
            return true;
        }
        '▕' => {
            rect(0.875, 0., 0.125, 1.);
            return true;
        }
        '▖'..='▟' => {
            // Quadrants: bit 0 upper-left, 1 upper-right, 2 lower-left,
            // 3 lower-right.
            let quadrants = match ch {
                '▖' => 0b0100,
                '▗' => 0b1000,
                '▘' => 0b0001,
                '▙' => 0b1101,
                '▚' => 0b1001,
                '▛' => 0b0111,
                '▜' => 0b1011,
                '▝' => 0b0010,
                '▞' => 0b0110,
                _ => 0b1110, // '▟'
            };
            for i in 0..4 {
                if quadrants & (1 << i) != 0 {
                    rect((i % 2) as f32 * 0.5, (i / 2) as f32 * 0.5, 0.5, 0.5);
                }
            }
            return true;
        }
        _ => {}
    }

    // Lines: arms (left, right, up, down) with weight 0 = none, 1 = light,
    // 2 = heavy.
    let arms: [u8; 4] = match ch {
        '─' => [1, 1, 0, 0],
        '━' => [2, 2, 0, 0],
        '│' => [0, 0, 1, 1],
        '┃' => [0, 0, 2, 2],
        '┌' | '╭' => [0, 1, 0, 1],
        '┐' | '╮' => [1, 0, 0, 1],
        '└' | '╰' => [0, 1, 1, 0],
        '┘' | '╯' => [1, 0, 1, 0],
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
        _ => return false,
    };
    let stroke = |weight: u8| if weight == 2 { px(2.) } else { px(1.) };
    let cx = x0 + w * 0.5;
    let cy = h * 0.5;
    let rounded = matches!(ch, '╭' | '╮' | '╯' | '╰');
    if rounded {
        let [left, right, up, _] = arms;
        let from = if up == 1 {
            point(cx, px(0.))
        } else {
            point(cx, h)
        };
        let to = if left == 1 {
            point(x0, cy)
        } else if right == 1 {
            point(x0 + w, cy)
        } else {
            point(cx, cy)
        };
        shapes.push(Shape::Arc {
            from,
            center: point(cx, cy),
            to,
            width: px(1.),
            color,
        });
        return true;
    }
    let max_weight = arms.iter().copied().max().unwrap_or(1);
    let half = |weight: u8| stroke(weight) / 2.;
    let [left, right, up, down] = arms;
    // Horizontal arms extend through the center so they meet vertical ones.
    let reach_v = half(max_weight.max(up.max(down)));
    let reach_h = half(max_weight.max(left.max(right)));
    if left > 0 {
        let t = stroke(left);
        shapes.push(Shape::Rect(
            Bounds::new(point(x0, cy - t / 2.), size(cx - x0 + reach_v, t)),
            color,
        ));
    }
    if right > 0 {
        let t = stroke(right);
        shapes.push(Shape::Rect(
            Bounds::new(
                point(cx - reach_v, cy - t / 2.),
                size(x0 + w - cx + reach_v, t),
            ),
            color,
        ));
    }
    if up > 0 {
        let t = stroke(up);
        shapes.push(Shape::Rect(
            Bounds::new(point(cx - t / 2., px(0.)), size(t, cy + reach_h)),
            color,
        ));
    }
    if down > 0 {
        let t = stroke(down);
        shapes.push(Shape::Rect(
            Bounds::new(point(cx - t / 2., cy - reach_h), size(t, h - cy + reach_h)),
            color,
        ));
    }
    true
}
