//! [`TerminalView`]: the entity that owns the VT engine, turns PTY bytes into
//! frames, and turns keyboard, mouse and clipboard input into bytes for the
//! PTY ([`TerminalEvent::Input`]).
//!
//! The view never touches a process. The host feeds output with
//! [`TerminalView::feed`] and forwards [`TerminalEvent`]s to whatever owns the
//! PTY (the daemon, in Agentloft).
//!
//! Frame flow: `feed` queues bytes and schedules one flush task; the task
//! parses in time-boxed slices, yielding between slices so a 50 MB `cat`
//! never blocks input or painting, and calls `notify` once per slice. GPUI
//! coalesces notifications into at most one frame; at prepaint the element
//! asks the engine for damaged rows only and re-shapes just those.
//!
//! Patterns adapted from Ghostex's `terminal_element.rs` (MIT, see
//! THIRD_PARTY_NOTICES.md).

use crate::backend::ghostty::GhosttyBackend;
use crate::backend::{
    Colors, Damage, Effect, Frame, Geometry, GridPoint, KeyAction, KeyInput, MouseAction,
    MouseButton as VtButton, MouseInput, Scroll, TerminalBackend,
};
use crate::element::{CellMetrics, FontConfig, RowLayout, TerminalElement};
use crate::input::{self, KeyRoute, RouteContext};
use crate::theme;
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    Global, InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction,
    ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollEvent, SharedString, Styled as _,
    Subscription, Task, UTF16Selection, Window, div, point, px, size,
};
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

gpui_kit::actions!(
    loft_terminal,
    [
        /// Copy the selection to the clipboard.
        Copy,
        /// Paste the clipboard into the terminal.
        Paste,
        /// Select all scrollback and screen text.
        SelectAll
    ]
);

/// Key context set on a focused terminal.
pub const KEY_CONTEXT: &str = "Terminal";

/// Parse at most this many bytes before checking the time budget.
const FEED_CHUNK: usize = 64 * 1024;
/// Time budget for one parse slice before yielding to input and painting.
const FEED_BUDGET: Duration = Duration::from_millis(6);
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(600);
/// Rows a discrete mouse-wheel line scrolls (Ghostty's default multiplier).
const WHEEL_LINES_MULTIPLIER: f32 = 3.;

/// Events for the host. `Input` bytes must be written to the PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes to write to the PTY: keys, paste, mouse reports and replies
    /// to terminal queries.
    Input(Vec<u8>),
    /// The grid size changed; resize the PTY (`TIOCSWINSZ`). Pixel sizes
    /// are in device pixels.
    Resize {
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    /// The program set the window title (OSC 0/2).
    TitleChanged(String),
    /// The program rang the bell (BEL).
    Bell,
    /// ⌘-click on a URL in the grid.
    OpenUrl(String),
}

const CTRL_LETTERS: [&str; 26] = [
    "ctrl-a", "ctrl-b", "ctrl-c", "ctrl-d", "ctrl-e", "ctrl-f", "ctrl-g", "ctrl-h", "ctrl-i",
    "ctrl-j", "ctrl-k", "ctrl-l", "ctrl-m", "ctrl-n", "ctrl-o", "ctrl-p", "ctrl-q", "ctrl-r",
    "ctrl-s", "ctrl-t", "ctrl-u", "ctrl-v", "ctrl-w", "ctrl-x", "ctrl-y", "ctrl-z",
];

struct Initialized;
impl Global for Initialized {}

/// Register the terminal's default key bindings. Called by
/// [`TerminalView::new`]; idempotent.
pub fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    let ctx = Some(KEY_CONTEXT);
    let mut bindings = if cfg!(target_os = "macos") {
        vec![
            KeyBinding::new("cmd-c", Copy, ctx),
            KeyBinding::new("cmd-v", Paste, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
        ]
    } else {
        vec![
            KeyBinding::new("ctrl-shift-c", Copy, ctx),
            KeyBinding::new("ctrl-shift-v", Paste, ctx),
            KeyBinding::new("shift-insert", Paste, ctx),
            KeyBinding::new("ctrl-shift-a", SelectAll, ctx),
        ]
    };
    // Action bindings win over raw key listeners in GPUI, and ancestors bind
    // some of these (gpui-kit's Root moves focus on tab / shift-tab). Inside
    // a terminal they belong to the running program.
    for key in [
        "tab",
        "shift-tab",
        "escape",
        "enter",
        "shift-enter",
        "alt-enter",
        "ctrl-enter",
        "space",
        "backspace",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "ctrl-space",
    ] {
        bindings.push(KeyBinding::new(key, NoAction, ctx));
    }
    for key in CTRL_LETTERS {
        bindings.push(KeyBinding::new(key, NoAction, ctx));
    }
    cx.bind_keys(bindings);
}

/// Grid geometry from the last layout, for mapping pointer positions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Grid {
    pub origin: Point<Pixels>,
    pub bounds: Bounds<Pixels>,
    pub metrics: CellMetrics,
    pub cols: u16,
    pub rows: u16,
    pub scale: f32,
}

impl Grid {
    pub fn geometry(&self) -> Geometry {
        Geometry {
            cols: self.cols,
            rows: self.rows,
            cell_width: self.metrics.device_cell_width(self.scale),
            cell_height: self.metrics.device_line_height(self.scale),
        }
    }

    /// Viewport cell under `position`, clamped into the grid.
    pub fn cell_at(&self, position: Point<Pixels>) -> (u16, u16) {
        let x = (position.x - self.origin.x) / self.metrics.cell_width;
        let y = (position.y - self.origin.y) / self.metrics.line_height;
        let col = x.floor().clamp(0., f32::from(self.cols.saturating_sub(1)));
        let row = y.floor().clamp(0., f32::from(self.rows.saturating_sub(1)));
        (col as u16, row as u16)
    }

    /// Pointer position in device pixels relative to the grid origin,
    /// proportional to the rounded device cell size so reports line up with
    /// the cell sizes given to the PTY.
    pub fn device_position(&self, position: Point<Pixels>) -> (f32, f32) {
        let geometry = self.geometry();
        let cw = geometry.cell_width as f32;
        let ch = geometry.cell_height as f32;
        let x = (position.x - self.origin.x) / self.metrics.cell_width * cw;
        let y = (position.y - self.origin.y) / self.metrics.line_height * ch;
        (
            x.clamp(0., (geometry.width() as f32 - 1.).max(0.)),
            y.clamp(0., (geometry.height() as f32 - 1.).max(0.)),
        )
    }

    pub fn grid_point(&self, position: Point<Pixels>) -> GridPoint {
        let (col, row) = self.cell_at(position);
        let (x, y) = self.device_position(position);
        GridPoint { col, row, x, y }
    }
}

#[derive(Default)]
struct MouseState {
    /// A left-button selection gesture is in progress.
    selecting: bool,
    /// Buttons pressed while reporting to the program.
    reported_buttons: Vec<VtButton>,
    /// Sub-row wheel remainder in rows.
    wheel_remainder: f32,
}

/// A GPUI terminal widget backed by libghostty-vt.
pub struct TerminalView {
    backend: Box<dyn TerminalBackend>,
    focus_handle: FocusHandle,
    pub(crate) frame: Frame,
    pub(crate) rows_cache: Vec<Option<Rc<RowLayout>>>,
    force_full: bool,
    pub(crate) font: FontConfig,
    pub(crate) metrics: Option<(FontConfig, f32, CellMetrics)>,
    pub(crate) grid: Option<Grid>,
    reported_size: Option<(u16, u16, u16, u16)>,
    pending: VecDeque<Vec<u8>>,
    pending_offset: usize,
    flushing: bool,
    colors: Option<Colors>,
    pub(crate) marked_text: Option<String>,
    marked_selection: Option<Range<usize>>,
    /// The key whose text the platform is about to commit.
    text_key: Option<(Keystroke, bool)>,
    option_as_alt: bool,
    mouse: MouseState,
    pub(crate) focused: bool,
    pub(crate) blink_visible: bool,
    blink_task: Option<Task<()>>,
    sync_retry: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        init(cx);
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.focus_changed(true, cx)
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.focus_changed(false, cx)
            }),
        ];
        let backend = GhosttyBackend::new(80, 24).expect("libghostty-vt terminal allocation");
        Self {
            backend: Box::new(backend),
            focus_handle,
            frame: Frame::default(),
            rows_cache: Vec::new(),
            force_full: true,
            font: FontConfig::default(),
            metrics: None,
            grid: None,
            reported_size: None,
            pending: VecDeque::new(),
            pending_offset: 0,
            flushing: false,
            colors: None,
            marked_text: None,
            marked_selection: None,
            text_key: None,
            option_as_alt: !cfg!(target_os = "macos"),
            mouse: MouseState::default(),
            focused: false,
            blink_visible: true,
            blink_task: None,
            sync_retry: None,
            _subscriptions: subscriptions,
        }
    }

    /// Queue PTY output. Parsing happens in time-boxed slices on the next
    /// turns of the event loop; painting happens at most once per frame.
    pub fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.pending.push_back(bytes.to_vec());
        if self.flushing {
            return;
        }
        self.flushing = true;
        cx.spawn(async move |this, cx| {
            loop {
                let more = this
                    .update(cx, |this, cx| this.flush_pending(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
                // Yield so input and frames run between slices.
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
        })
        .detach();
    }

    /// Clear the screen, scrollback, modes and title, e.g. before replaying
    /// a buffered PTY stream into a fresh view state.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.pending_offset = 0;
        self.backend.reset();
        self.colors = None;
        self.marked_text = None;
        self.marked_selection = None;
        self.text_key = None;
        self.mouse = MouseState::default();
        self.invalidate();
        cx.notify();
    }

    /// Set the monospace font. Cell metrics, and so the grid size, follow.
    pub fn set_font(&mut self, family: SharedString, size: Pixels, cx: &mut Context<Self>) {
        let font = FontConfig { family, size };
        if font != self.font {
            self.font = font;
            self.metrics = None;
            self.invalidate();
            cx.notify();
        }
    }

    /// Treat Option (macOS) as Alt/Meta: Option-b sends `ESC b` instead of
    /// composing "∫". Off by default on macOS, always on elsewhere.
    pub fn set_option_as_alt(&mut self, enabled: bool) {
        self.option_as_alt = enabled;
        self.backend.set_option_as_alt(enabled);
    }

    /// The grid size `(cols, rows)`.
    pub fn grid_size(&self) -> (u16, u16) {
        self.backend.size()
    }

    /// The title set by the running program, if any.
    pub fn title(&self) -> Option<&str> {
        self.backend.title()
    }

    /// The working directory reported via OSC 7, if any.
    pub fn working_directory(&self) -> Option<&str> {
        self.backend.pwd()
    }

    /// The selected text, if any.
    pub fn selection_text(&self) -> Option<String> {
        self.backend.selection_text()
    }

    /// Text of viewport row `row` as last rendered (trailing blanks trimmed).
    pub fn row_text(&self, row: usize) -> String {
        self.frame.row_text(row)
    }

    /// Window-space bounds of a viewport cell, from the last layout. Useful
    /// for anchoring popovers to the cursor or driving pointer input.
    pub fn cell_bounds(&self, col: u16, row: u16) -> Option<Bounds<Pixels>> {
        let grid = self.grid?;
        (col < grid.cols && row < grid.rows).then(|| {
            let m = grid.metrics;
            Bounds::new(
                grid.origin
                    + point(
                        m.cell_width * f32::from(col),
                        m.line_height * f32::from(row),
                    ),
                size(m.cell_width, m.line_height),
            )
        })
    }

    /// The cursor position `(col, row)` in the viewport, if visible there.
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        self.frame.cursor.position
    }

    /// Scroll the viewport by `rows` (negative is toward history).
    pub fn scroll_by(&mut self, rows: isize, cx: &mut Context<Self>) {
        self.backend.scroll(Scroll::Delta(rows));
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.backend.scroll(Scroll::Top);
        cx.notify();
    }

    pub fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.backend.scroll(Scroll::Bottom);
        cx.notify();
    }

    /// Paste text as if from the clipboard (bracketed when enabled).
    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = self.backend.encode_paste(text);
        self.send_input(bytes, cx);
    }

    fn invalidate(&mut self) {
        self.force_full = true;
        self.rows_cache.clear();
    }

    /// Parse one time-boxed slice of queued output. Returns whether more
    /// remains.
    fn flush_pending(&mut self, cx: &mut Context<Self>) -> bool {
        let start = Instant::now();
        while let Some(chunk) = self.pending.front() {
            let end = (self.pending_offset + FEED_CHUNK).min(chunk.len());
            self.backend.write(&chunk[self.pending_offset..end]);
            if end == chunk.len() {
                self.pending.pop_front();
                self.pending_offset = 0;
            } else {
                self.pending_offset = end;
            }
            if start.elapsed() >= FEED_BUDGET {
                break;
            }
        }
        self.dispatch_effects(cx);
        cx.notify();
        let more = !self.pending.is_empty();
        self.flushing = more;
        more
    }

    fn dispatch_effects(&mut self, cx: &mut Context<Self>) {
        for effect in self.backend.take_effects() {
            match effect {
                Effect::PtyWrite(bytes) => cx.emit(TerminalEvent::Input(bytes)),
                Effect::Bell => cx.emit(TerminalEvent::Bell),
                Effect::TitleChanged(title) => cx.emit(TerminalEvent::TitleChanged(title)),
                Effect::PwdChanged(_) => {}
                Effect::ClipboardWrite(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
            }
        }
    }

    /// Send input to the PTY: the user is interacting, so drop the
    /// selection, jump back to the live screen and show the cursor.
    fn send_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        if self.backend.has_selection() {
            self.backend.clear_selection();
        }
        if !self.backend.is_at_bottom() {
            self.backend.scroll(Scroll::Bottom);
        }
        self.restart_blink(cx);
        cx.emit(TerminalEvent::Input(bytes));
        cx.notify();
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        self.focused = focused;
        let bytes = self.backend.encode_focus(focused);
        if !bytes.is_empty() {
            cx.emit(TerminalEvent::Input(bytes));
        }
        if focused {
            self.restart_blink(cx);
        } else {
            self.blink_task = None;
            self.blink_visible = true;
        }
        cx.notify();
    }

    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_visible = true;
        if !self.focused {
            self.blink_task = None;
            return;
        }
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CURSOR_BLINK_INTERVAL).await;
                let keep = this
                    .update(cx, |this, cx| {
                        if !this.focused {
                            return false;
                        }
                        if this.frame.cursor.blinking || !this.blink_visible {
                            this.blink_visible = !this.blink_visible;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    // ----- layout (called from the element's prepaint) -----

    /// Sync theme colors into the engine. Called from `render`.
    fn sync_colors(&mut self, cx: &App) {
        let colors = theme::colors(cx);
        if self.colors.as_ref() != Some(&colors) {
            self.backend.set_colors(&colors);
            self.colors = Some(colors);
            self.force_full = true;
        }
    }

    pub(crate) fn cell_metrics(&mut self, window: &mut Window) -> CellMetrics {
        let scale = window.scale_factor();
        if let Some((font, metrics_scale, metrics)) = &self.metrics
            && *font == self.font
            && *metrics_scale == scale
        {
            return *metrics;
        }
        let metrics = CellMetrics::measure(&self.font, window);
        self.metrics = Some((self.font.clone(), scale, metrics));
        self.rows_cache.clear();
        metrics
    }

    /// Fit the grid to `bounds`, resize the engine (emitting `Resize` once
    /// per change), and pull damaged rows from the engine.
    pub(crate) fn sync_layout(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Damage {
        let metrics = self.cell_metrics(window);
        let scale = window.scale_factor();
        let padding = crate::element::PADDING;
        let inner_w = (bounds.size.width - padding.width * 2.).max(px(0.));
        let inner_h = (bounds.size.height - padding.height * 2.).max(px(0.));
        let cols = ((inner_w / metrics.cell_width).floor() as u16).max(2);
        let rows = ((inner_h / metrics.line_height).floor() as u16).max(1);
        let cell_w = metrics.device_cell_width(scale);
        let cell_h = metrics.device_line_height(scale);
        let report = (
            cols,
            rows,
            u16::try_from(u32::from(cols) * cell_w).unwrap_or(u16::MAX),
            u16::try_from(u32::from(rows) * cell_h).unwrap_or(u16::MAX),
        );
        if self.reported_size != Some(report) {
            self.backend.resize(cols, rows, cell_w, cell_h);
            self.reported_size = Some(report);
            self.dispatch_effects(cx);
            cx.emit(TerminalEvent::Resize {
                cols,
                rows,
                pixel_width: report.2,
                pixel_height: report.3,
            });
        }
        self.grid = Some(Grid {
            origin: bounds.origin + point(padding.width, padding.height),
            bounds,
            metrics,
            cols,
            rows,
            scale,
        });

        let damage = self
            .backend
            .update_frame(&mut self.frame, std::mem::take(&mut self.force_full));
        let rows = usize::from(self.frame.rows);
        if self.rows_cache.len() != rows {
            self.rows_cache.clear();
            self.rows_cache.resize(rows, None);
        }
        match &damage {
            Damage::None => {}
            Damage::Full => self.rows_cache.iter_mut().for_each(|r| *r = None),
            Damage::Rows(damaged) => {
                for &row in damaged {
                    if let Some(slot) = self.rows_cache.get_mut(usize::from(row)) {
                        *slot = None;
                    }
                }
            }
        }
        // Synchronized output (DECSET 2026) keeps the old frame until the
        // program ends the update or the hold times out; come back to check.
        if damage.is_none() && self.backend.is_render_held() {
            self.schedule_sync_retry(cx);
        }
        damage
    }

    fn schedule_sync_retry(&mut self, cx: &mut Context<Self>) {
        if self.sync_retry.is_some() {
            return;
        }
        self.sync_retry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            this.update(cx, |this, cx| {
                this.sync_retry = None;
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn scrollbar(&self) -> Option<crate::backend::Scrollbar> {
        (!self.backend.is_at_bottom()).then(|| self.backend.scrollbar())
    }

    pub(crate) fn selection_rows(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        self.frame
            .lines
            .iter()
            .enumerate()
            .filter_map(|(row, line)| line.selection.map(|(s, e)| (row as u16, s, e)))
    }

    // ----- keyboard -----

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = RouteContext {
            is_held: event.is_held,
            caps_lock: window.capslock().on,
            option_as_alt: self.option_as_alt,
            composing: self.marked_text.is_some(),
        };
        self.text_key = None;
        match input::route_key_down(&event.keystroke, ctx) {
            KeyRoute::Text => {
                // The platform text system commits the text next.
                self.text_key = Some((event.keystroke.clone(), ctx.caps_lock));
            }
            KeyRoute::Encode(key) => {
                let bytes = self.backend.encode_key(&key);
                if !bytes.is_empty() {
                    self.send_input(bytes, cx);
                    cx.stop_propagation();
                }
            }
            KeyRoute::Bytes(bytes) => {
                self.send_input(bytes.to_vec(), cx);
                cx.stop_propagation();
            }
            KeyRoute::Ignore => {}
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() || event.keystroke.modifiers.platform {
            return;
        }
        let key = input::key_up_input(&event.keystroke, window.capslock().on);
        let bytes = self.backend.encode_key(&key);
        if !bytes.is_empty() {
            cx.emit(TerminalEvent::Input(bytes));
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        match self.backend.selection_text() {
            Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            None => cx.propagate(),
        }
    }

    fn paste_clipboard(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            && !text.is_empty()
        {
            self.paste(&text, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.backend.select_all();
        cx.notify();
    }

    // ----- mouse (called from the element's listeners) -----

    fn reporting_mouse(&self, modifiers: &Modifiers) -> bool {
        // Shift is the conventional override to select text in mouse apps.
        !modifiers.shift && self.backend.modes().mouse_tracking
    }

    fn report_mouse(&mut self, input: MouseInput, cx: &mut Context<Self>) {
        let Some(grid) = self.grid else { return };
        let bytes = self.backend.encode_mouse(&input, grid.geometry());
        if !bytes.is_empty() {
            cx.emit(TerminalEvent::Input(bytes));
        }
    }

    pub(crate) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(grid) = self.grid else { return };
        // ⌘-click opens a URL under the pointer (before mouse reporting, so
        // it works inside TUIs too).
        if event.button == MouseButton::Left && event.modifiers.platform {
            let (col, row) = grid.cell_at(event.position);
            if let Some(url) = crate::links::url_at(&self.row_text(row as usize), col as usize) {
                cx.emit(TerminalEvent::OpenUrl(url));
                return;
            }
        }
        if self.reporting_mouse(&event.modifiers) {
            let Some(button) = vt_button(event.button) else {
                return;
            };
            if !self.mouse.reported_buttons.contains(&button) {
                self.mouse.reported_buttons.push(button);
            }
            let (x, y) = grid.device_position(event.position);
            self.report_mouse(
                MouseInput {
                    action: MouseAction::Press,
                    button: Some(button),
                    mods: input::mods(&event.modifiers, false),
                    x,
                    y,
                    any_button_pressed: true,
                },
                cx,
            );
            return;
        }
        if event.button == MouseButton::Left {
            self.mouse.selecting = true;
            let time = window_time();
            self.backend
                .selection_press(grid.grid_point(event.position), time);
            cx.notify();
        }
    }

    pub(crate) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.grid else { return };
        if self.mouse.selecting && event.pressed_button != Some(MouseButton::Left) {
            // The button went up outside the window; finish the gesture.
            self.mouse.selecting = false;
            self.backend.selection_release(None);
        }
        if self.mouse.selecting {
            // Dragging past the top or bottom edge scrolls the viewport.
            if event.position.y < grid.origin.y {
                self.backend.scroll(Scroll::Delta(-1));
            } else if event.position.y > grid.origin.y + grid.metrics.line_height * grid.rows as f32
            {
                self.backend.scroll(Scroll::Delta(1));
            }
            let rectangle = event.modifiers.alt;
            self.backend.selection_drag(
                grid.grid_point(event.position),
                grid.geometry(),
                rectangle,
            );
            cx.notify();
            return;
        }
        if (hovered || !self.mouse.reported_buttons.is_empty())
            && self.reporting_mouse(&event.modifiers)
        {
            let (x, y) = grid.device_position(event.position);
            self.report_mouse(
                MouseInput {
                    action: MouseAction::Motion,
                    button: self.mouse.reported_buttons.first().copied(),
                    mods: input::mods(&event.modifiers, false),
                    x,
                    y,
                    any_button_pressed: !self.mouse.reported_buttons.is_empty(),
                },
                cx,
            );
        }
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(grid) = self.grid else { return };
        if self.mouse.selecting && event.button == MouseButton::Left {
            self.mouse.selecting = false;
            self.backend
                .selection_release(Some(grid.grid_point(event.position)));
            cx.notify();
            return;
        }
        let Some(button) = vt_button(event.button) else {
            return;
        };
        let Some(index) = self
            .mouse
            .reported_buttons
            .iter()
            .position(|b| *b == button)
        else {
            return;
        };
        self.mouse.reported_buttons.remove(index);
        let (x, y) = grid.device_position(event.position);
        self.report_mouse(
            MouseInput {
                action: MouseAction::Release,
                button: Some(button),
                mods: input::mods(&event.modifiers, false),
                x,
                y,
                any_button_pressed: !self.mouse.reported_buttons.is_empty(),
            },
            cx,
        );
    }

    pub(crate) fn scroll_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let Some(grid) = self.grid else { return };
        let rows_delta = match event.delta {
            ScrollDelta::Pixels(delta) => delta.y / grid.metrics.line_height,
            ScrollDelta::Lines(delta) => delta.y * WHEEL_LINES_MULTIPLIER,
        };
        let total = self.mouse.wheel_remainder + rows_delta;
        let rows = total.trunc();
        self.mouse.wheel_remainder = total - rows;
        if rows == 0. {
            return;
        }
        // Positive deltas scroll toward history (up).
        let up = rows > 0.;
        let count = rows.abs() as usize;
        let modes = self.backend.modes();
        if self.reporting_mouse(&event.modifiers) {
            let (x, y) = grid.device_position(event.position);
            let input = MouseInput {
                action: MouseAction::Press,
                button: Some(if up {
                    VtButton::WheelUp
                } else {
                    VtButton::WheelDown
                }),
                mods: input::mods(&event.modifiers, false),
                x,
                y,
                any_button_pressed: !self.mouse.reported_buttons.is_empty(),
            };
            let mut bytes = Vec::new();
            for _ in 0..count {
                bytes.extend(self.backend.encode_mouse(&input, grid.geometry()));
            }
            if !bytes.is_empty() {
                cx.emit(TerminalEvent::Input(bytes));
            }
        } else if modes.alt_screen && modes.alternate_scroll && !event.modifiers.shift {
            // Pagers and editors on the alternate screen get arrow keys.
            let key = KeyInput {
                key: if up { "up" } else { "down" }.into(),
                action: KeyAction::Press,
                ..KeyInput::default()
            };
            let one = self.backend.encode_key(&key);
            if !one.is_empty() {
                cx.emit(TerminalEvent::Input(one.repeat(count)));
            }
        } else {
            let delta = count as isize;
            self.backend
                .scroll(Scroll::Delta(if up { -delta } else { delta }));
            cx.notify();
        }
    }
}

fn vt_button(button: MouseButton) -> Option<VtButton> {
    match button {
        MouseButton::Left => Some(VtButton::Left),
        MouseButton::Middle => Some(VtButton::Middle),
        MouseButton::Right => Some(VtButton::Right),
        _ => None,
    }
}

/// A monotonic timestamp for click-count detection.
fn window_time() -> Duration {
    thread_local! {
        static EPOCH: Instant = Instant::now();
    }
    EPOCH.with(|epoch| epoch.elapsed())
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_colors(cx);
        let background = theme::to_hsla(
            self.colors
                .as_ref()
                .map(|c| c.background)
                .unwrap_or_default(),
        );
        div()
            .id("loft-terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_action(cx.listener(Self::select_all))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .child(TerminalElement::new(cx.entity()))
    }
}

/// IME integration: composition renders as marked text at the cursor,
/// commits go to the PTY (encoded with the key that produced them), and
/// the candidate window anchors at the cursor cell. The terminal has no
/// editable document, so ranges only address the marked text.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.marked_text.as_ref()?;
        let len = text.encode_utf16().count();
        let start = range.start.min(len);
        let end = range.end.clamp(start, len);
        if start == end {
            return None;
        }
        if (start..end) != range {
            *adjusted_range = Some(start..end);
        }
        Some(text[utf16_to_byte(text, start)..utf16_to_byte(text, end)].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selection.clone().unwrap_or(0..0),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_selection = None;
        if self.marked_text.take().is_some() {
            cx.notify();
        }
        window.invalidate_character_coordinates();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let had_marked = self.marked_text.take().is_some();
        self.marked_selection = None;
        let key = self.text_key.take();
        if !text.is_empty() {
            let bytes = match key {
                Some((keystroke, caps_lock)) => {
                    let input = input::committed_text_input(&keystroke, caps_lock, text);
                    let encoded = self.backend.encode_key(&input);
                    if encoded.is_empty() {
                        text.as_bytes().to_vec()
                    } else {
                        encoded
                    }
                }
                None => text.as_bytes().to_vec(),
            };
            self.send_input(bytes, cx);
        } else if had_marked {
            cx.notify();
        }
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The key AppKit is composing with is not sent; the composed text is.
        self.text_key = None;
        if new_text.is_empty() {
            self.marked_text = None;
            self.marked_selection = None;
        } else {
            let len = new_text.encode_utf16().count();
            self.marked_text = Some(new_text.to_string());
            self.marked_selection = Some(match new_selected_range {
                Some(range) => {
                    let start = range.start.min(len);
                    start..range.end.clamp(start, len)
                }
                None => len..len,
            });
        }
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let grid = self.grid?;
        let (col, row) = self.frame.cursor.position?;
        let m = grid.metrics;
        let origin = grid.origin
            + point(
                m.cell_width * (f32::from(col) + range_utf16.start as f32),
                m.line_height * f32::from(row),
            );
        Some(Bounds::new(origin, size(m.cell_width, m.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut utf16 = 0;
    for (byte, c) in text.char_indices() {
        if utf16 >= offset {
            return byte;
        }
        utf16 += c.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::utf16_to_byte;

    #[test]
    fn utf16_offsets_map_to_bytes() {
        let text = "aé😀b";
        assert_eq!(utf16_to_byte(text, 0), 0);
        assert_eq!(utf16_to_byte(text, 1), 1);
        assert_eq!(utf16_to_byte(text, 2), 3);
        assert_eq!(utf16_to_byte(text, 4), 7);
        assert_eq!(utf16_to_byte(text, 9), text.len());
    }
}
