//! [`TerminalView`]: the entity that owns the VT engine, turns PTY output
//! into frames and keyboard, mouse and clipboard input into
//! [`TerminalEvent::Input`] bytes for the PTY.
//!
//! Output is parsed in time-boxed slices so a large burst never holds up
//! input or painting. The element ([`crate::element`]) calls
//! [`TerminalView::sync_layout`] at prepaint, which fits the grid to the
//! element, resizes the engine and pulls the damaged rows into `frame`.

use crate::backend::ghostty::GhosttyBackend;
use crate::backend::{
    Colors, Damage, Effect, Frame, Geometry, GridPoint, KeyAction, KeyInput, MouseAction,
    MouseButton as TermButton, MouseInput, Scroll, Scrollbar, TerminalBackend,
};
use crate::element::{CellMetrics, CursorGlyph, FontConfig, PADDING, RowLayout, TerminalElement};
use crate::input::{self, KeyRoute, RouteContext};
use crate::theme;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    Global, InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction,
    ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollEvent, SharedString, Styled as _,
    Subscription, Task, UTF16Selection, WeakEntity, Window, div, point, size,
};
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;
use web_time::Instant;

gpui_kit::actions!(
    loft_terminal,
    [
        /// Copy the selection to the clipboard.
        Copy,
        /// Paste the clipboard into the terminal.
        Paste,
        /// Select all scrollback and screen text.
        SelectAll,
        /// Hand the selection to the host, for the message being written
        /// ([`TerminalEvent::AddToMessage`]). Unbound here: the host binds
        /// the keys it wants in [`KEY_CONTEXT`].
        AddSelectionToMessage
    ]
);

/// Key context set on a focused terminal.
pub const KEY_CONTEXT: &str = "Terminal";

/// Engine size before the first layout.
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
/// Most output written to the engine in one go.
const FEED_PIECE: usize = 64 * 1024;
/// How long one parsing slice may run before yielding.
const SLICE_BUDGET: Duration = Duration::from_millis(6);
/// Pause between slices, so input and frames get their turn.
const SLICE_PAUSE: Duration = Duration::from_millis(1);
const BLINK_INTERVAL: Duration = Duration::from_millis(600);
/// Rows per line of a line-based wheel delta.
const WHEEL_ROWS_PER_LINE: f32 = 3.;
/// While synchronized output holds rendering, look again after this long.
const SYNC_RECHECK: Duration = Duration::from_millis(100);

/// Events for the host. `Input` bytes must be written to the PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes to write to the PTY: keys, paste, mouse reports and replies
    /// to terminal queries.
    Input(Vec<u8>),
    /// The grid size changed; resize the PTY. Pixel sizes are in device
    /// pixels.
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
    /// The selected text, asked to go into the message the user is writing
    /// ([`AddSelectionToMessage`]). The terminal doesn't know what a
    /// message is; the host puts it there.
    AddToMessage(String),
}

/// Marks the key bindings as registered.
struct Bindings;

impl Global for Bindings {}

/// Register the terminal's default key bindings. Idempotent.
pub fn init(cx: &mut App) {
    if cx.has_global::<Bindings>() {
        return;
    }
    cx.set_global(Bindings);
    let ctx = Some(KEY_CONTEXT);
    let mut bindings = Vec::new();
    if cfg!(target_arch = "wasm32") {
        // Pasting is the browser's own paste event (`EntityInputHandler::paste`).
        bindings.extend([
            KeyBinding::new("cmd-c", Copy, ctx),
            KeyBinding::new("ctrl-shift-c", Copy, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
            KeyBinding::new("ctrl-shift-a", SelectAll, ctx),
        ]);
    } else if cfg!(target_os = "macos") {
        bindings.extend([
            KeyBinding::new("cmd-c", Copy, ctx),
            KeyBinding::new("cmd-v", Paste, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("ctrl-shift-c", Copy, ctx),
            KeyBinding::new("ctrl-shift-v", Paste, ctx),
            KeyBinding::new("shift-insert", Paste, ctx),
            KeyBinding::new("ctrl-shift-a", SelectAll, ctx),
        ]);
    }
    // Keys the shell needs, kept from bindings further up (Root moving focus
    // on tab, a list's enter, ...).
    const OURS: &[&str] = &[
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
        "ctrl-a",
        "ctrl-b",
        "ctrl-c",
        "ctrl-d",
        "ctrl-e",
        "ctrl-f",
        "ctrl-g",
        "ctrl-h",
        "ctrl-i",
        "ctrl-j",
        "ctrl-k",
        "ctrl-l",
        "ctrl-m",
        "ctrl-n",
        "ctrl-o",
        "ctrl-p",
        "ctrl-q",
        "ctrl-r",
        "ctrl-s",
        "ctrl-t",
        "ctrl-u",
        "ctrl-v",
        "ctrl-w",
        "ctrl-x",
        "ctrl-y",
        "ctrl-z",
    ];
    bindings.extend(OURS.iter().map(|keys| KeyBinding::new(keys, NoAction, ctx)));
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

    /// Position relative to the grid origin, in cells (fractional).
    fn cells(&self, position: Point<Pixels>) -> (f32, f32) {
        (
            (position.x - self.origin.x) / self.metrics.cell_width,
            (position.y - self.origin.y) / self.metrics.line_height,
        )
    }

    pub fn cell_at(&self, position: Point<Pixels>) -> (u16, u16) {
        let (x, y) = self.cells(position);
        let clamp = |v: f32, n: u16| (v.floor().max(0.) as u32).min(u32::from(n.max(1)) - 1) as u16;
        (clamp(x, self.cols), clamp(y, self.rows))
    }

    pub fn device_position(&self, position: Point<Pixels>) -> (f32, f32) {
        let geometry = self.geometry();
        let (x, y) = self.cells(position);
        let max_x = geometry.width().saturating_sub(1) as f32;
        let max_y = geometry.height().saturating_sub(1) as f32;
        (
            (x * geometry.cell_width as f32).clamp(0., max_x),
            (y * geometry.cell_height as f32).clamp(0., max_y),
        )
    }

    pub fn grid_point(&self, position: Point<Pixels>) -> GridPoint {
        let (col, row) = self.cell_at(position);
        let (x, y) = self.device_position(position);
        GridPoint { col, row, x, y }
    }
}

/// Monotonic time for the selection gesture's click counting.
fn gesture_time() -> Duration {
    thread_local! {
        static EPOCH: Instant = Instant::now();
    }
    EPOCH.with(Instant::elapsed)
}

/// A GPUI terminal widget backed by libghostty-vt.
pub struct TerminalView {
    pub(crate) frame: Frame,
    pub(crate) rows_cache: Vec<Option<Rc<RowLayout>>>,
    pub(crate) font: FontConfig,
    pub(crate) metrics: Option<(FontConfig, f32, CellMetrics)>,
    pub(crate) grid: Option<Grid>,
    pub(crate) marked_text: Option<String>,
    pub(crate) focused: bool,
    pub(crate) blink_visible: bool,
    /// The element's shaped glyph under a block cursor.
    pub(crate) cursor_glyph: CursorGlyph,

    backend: GhosttyBackend,
    focus_handle: FocusHandle,
    option_as_alt: bool,
    /// PTY output not parsed yet, and how far into its front chunk we are.
    queue: VecDeque<Vec<u8>>,
    queue_offset: usize,
    flushing: bool,
    /// The size last reported with [`TerminalEvent::Resize`].
    reported_size: Option<(u16, u16, u16, u16)>,
    /// The next frame is pulled with every row damaged.
    force_full: bool,
    /// Theme colours last pushed into the engine.
    colors: Option<Colors>,
    /// Selection inside `marked_text`, in UTF-16 offsets.
    marked_selection: Range<usize>,
    /// The fresh key press handed to the text system, with Caps Lock then:
    /// the text it commits is encoded as that key.
    pending_key: Option<(Keystroke, bool)>,
    selecting: bool,
    /// Buttons whose press went to the program, in press order.
    reported_buttons: Vec<TermButton>,
    wheel_remainder: f32,
    blink_task: Option<Task<()>>,
    sync_recheck: Option<Task<()>>,
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
        let mut backend = GhosttyBackend::new(INITIAL_COLS, INITIAL_ROWS)
            .expect("libghostty-vt could not allocate a terminal");
        let option_as_alt = !cfg!(target_os = "macos");
        backend.set_option_as_alt(option_as_alt);
        Self {
            frame: Frame::default(),
            rows_cache: Vec::new(),
            font: FontConfig::default(),
            metrics: None,
            grid: None,
            marked_text: None,
            focused: false,
            blink_visible: true,
            cursor_glyph: CursorGlyph::default(),
            backend,
            focus_handle,
            option_as_alt,
            queue: VecDeque::new(),
            queue_offset: 0,
            flushing: false,
            reported_size: None,
            force_full: true,
            colors: None,
            marked_selection: 0..0,
            pending_key: None,
            selecting: false,
            reported_buttons: Vec::new(),
            wheel_remainder: 0.,
            blink_task: None,
            sync_recheck: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        self.queue.push_back(bytes.to_vec());
        if self.flushing {
            return;
        }
        self.flushing = true;
        cx.spawn(async move |this, cx| {
            loop {
                let more = this
                    .update(cx, |this, cx| this.parse_slice(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
                cx.background_executor().timer(SLICE_PAUSE).await;
            }
        })
        .detach();
    }

    /// Parse queued output for up to [`SLICE_BUDGET`]. Returns whether
    /// output remains (and the flush task should go on).
    fn parse_slice(&mut self, cx: &mut Context<Self>) -> bool {
        let started = Instant::now();
        while let Some(chunk) = self.queue.front() {
            let end = (self.queue_offset + FEED_PIECE).min(chunk.len());
            self.backend.write(&chunk[self.queue_offset..end]);
            if end == chunk.len() {
                self.queue.pop_front();
                self.queue_offset = 0;
            } else {
                self.queue_offset = end;
            }
            if started.elapsed() >= SLICE_BUDGET {
                break;
            }
        }
        self.dispatch_effects(cx);
        cx.notify();
        self.flushing = !self.queue.is_empty();
        self.flushing
    }

    fn dispatch_effects(&mut self, cx: &mut Context<Self>) {
        for effect in self.backend.take_effects() {
            match effect {
                Effect::PtyWrite(bytes) => cx.emit(TerminalEvent::Input(bytes)),
                Effect::Bell => cx.emit(TerminalEvent::Bell),
                Effect::TitleChanged(title) => cx.emit(TerminalEvent::TitleChanged(title)),
                Effect::PwdChanged(_) => {}
                Effect::ClipboardWrite(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
        }
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        self.queue_offset = 0;
        self.backend.reset();
        self.colors = None;
        self.marked_text = None;
        self.marked_selection = 0..0;
        self.pending_key = None;
        self.selecting = false;
        self.reported_buttons.clear();
        self.wheel_remainder = 0.;
        self.force_full();
        cx.notify();
    }

    pub fn set_font(&mut self, family: SharedString, size: Pixels, cx: &mut Context<Self>) {
        let font = FontConfig { family, size };
        if font == self.font {
            return;
        }
        self.font = font;
        self.metrics = None;
        self.force_full();
        cx.notify();
    }

    pub fn set_option_as_alt(&mut self, enabled: bool) {
        self.option_as_alt = enabled;
        self.backend.set_option_as_alt(enabled);
    }

    pub fn grid_size(&self) -> (u16, u16) {
        self.backend.size()
    }

    pub fn title(&self) -> Option<&str> {
        self.backend.title()
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.backend.pwd()
    }

    pub fn selection_text(&self) -> Option<String> {
        self.backend.selection_text()
    }

    pub fn row_text(&self, row: usize) -> String {
        self.frame.row_text(row)
    }

    pub fn cell_bounds(&self, col: u16, row: u16) -> Option<Bounds<Pixels>> {
        let grid = self.grid?;
        if col >= grid.cols || row >= grid.rows {
            return None;
        }
        let m = grid.metrics;
        Some(Bounds::new(
            grid.origin
                + point(
                    m.cell_width * f32::from(col),
                    m.line_height * f32::from(row),
                ),
            size(m.cell_width, m.line_height),
        ))
    }

    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        self.frame.cursor.position
    }

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

    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = self.backend.encode_paste(text);
        self.send_input(bytes, cx);
    }

    // ----- input plumbing -----

    /// Bytes the user typed, pasted or committed: back to the live screen,
    /// selection gone, cursor shown.
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

    /// Bytes for the program that aren't the user's typing (reports).
    fn emit_raw(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !bytes.is_empty() {
            cx.emit(TerminalEvent::Input(bytes));
        }
    }

    fn force_full(&mut self) {
        self.force_full = true;
        self.rows_cache.iter_mut().for_each(|slot| *slot = None);
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        // Heard once from `render` and once from the subscription: act once.
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        let report = self.backend.encode_focus(focused);
        self.emit_raw(report, cx);
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
            return;
        }
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_INTERVAL).await;
                let go_on = this.update(cx, |this, cx| {
                    if !this.focused {
                        return false;
                    }
                    if this.frame.cursor.blinking || !this.blink_visible {
                        this.blink_visible = !this.blink_visible;
                        cx.notify();
                    }
                    true
                });
                if !matches!(go_on, Ok(true)) {
                    break;
                }
            }
        }));
    }

    // ----- layout (called from the element's prepaint) -----

    pub(crate) fn cell_metrics(&mut self, window: &mut Window) -> CellMetrics {
        let scale = window.scale_factor();
        if let Some((font, cached_scale, metrics)) = &self.metrics
            && *font == self.font
            && *cached_scale == scale
        {
            return *metrics;
        }
        let metrics = CellMetrics::measure(&self.font, window);
        self.metrics = Some((self.font.clone(), scale, metrics));
        self.rows_cache.iter_mut().for_each(|slot| *slot = None);
        metrics
    }

    pub(crate) fn sync_layout(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Damage {
        let metrics = self.cell_metrics(window);
        let scale = window.scale_factor().max(1.);
        let inner_w = (bounds.size.width - PADDING.width * 2.).max(Pixels::ZERO);
        let inner_h = (bounds.size.height - PADDING.height * 2.).max(Pixels::ZERO);
        let fit = |space: Pixels, cell: Pixels, min: u16| -> u16 {
            ((space / cell).floor().clamp(0., f32::from(u16::MAX)) as u16).max(min)
        };
        let cols = fit(inner_w, metrics.cell_width, 2);
        let rows = fit(inner_h, metrics.line_height, 1);
        let cell_w = metrics.device_cell_width(scale);
        let cell_h = metrics.device_line_height(scale);
        let pixels = |n: u16, cell: u32| (u32::from(n) * cell).min(u32::from(u16::MAX)) as u16;
        let reported = (cols, rows, pixels(cols, cell_w), pixels(rows, cell_h));
        if self.reported_size != Some(reported) {
            self.reported_size = Some(reported);
            self.backend.resize(cols, rows, cell_w, cell_h);
            self.dispatch_effects(cx);
            let (cols, rows, pixel_width, pixel_height) = reported;
            cx.emit(TerminalEvent::Resize {
                cols,
                rows,
                pixel_width,
                pixel_height,
            });
        }
        self.grid = Some(Grid {
            origin: bounds.origin + point(PADDING.width, PADDING.height),
            bounds,
            metrics,
            cols,
            rows,
            scale,
        });

        let force_full = std::mem::take(&mut self.force_full);
        let damage = self.backend.update_frame(&mut self.frame, force_full);
        let rows = usize::from(self.frame.rows);
        if self.rows_cache.len() != rows {
            self.rows_cache.clear();
            self.rows_cache.resize(rows, None);
        }
        match &damage {
            Damage::None => {}
            Damage::Full => self.rows_cache.iter_mut().for_each(|slot| *slot = None),
            Damage::Rows(rows) => {
                for &row in rows {
                    if let Some(slot) = self.rows_cache.get_mut(usize::from(row)) {
                        *slot = None;
                    }
                }
            }
        }

        // Synchronized output holds the frame back: look again soon, in
        // case the program never ends its update.
        if damage.is_none() && self.backend.is_render_held() && self.sync_recheck.is_none() {
            self.sync_recheck = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SYNC_RECHECK).await;
                let _ = this.update(cx, |this, cx| {
                    this.sync_recheck = None;
                    cx.notify();
                });
            }));
        }
        damage
    }

    pub(crate) fn scrollbar(&self) -> Option<Scrollbar> {
        (!self.backend.is_at_bottom()).then(|| self.backend.scrollbar())
    }

    pub(crate) fn selection_rows(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        self.frame
            .lines
            .iter()
            .enumerate()
            .filter_map(|(row, line)| {
                let (start, end) = line.selection?;
                Some((u16::try_from(row).ok()?, start, end))
            })
    }

    // ----- keys -----

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_key = None;
        let caps_lock = window.capslock().on;
        let ctx = RouteContext {
            is_held: event.is_held,
            caps_lock,
            option_as_alt: self.option_as_alt,
            composing: self.marked_text.is_some(),
        };
        match input::route_key_down(&event.keystroke, ctx) {
            KeyRoute::Text => self.pending_key = Some((event.keystroke.clone(), caps_lock)),
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

    fn key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() || event.keystroke.modifiers.platform {
            return;
        }
        let key = input::key_up_input(&event.keystroke, window.capslock().on);
        let bytes = self.backend.encode_key(&key);
        self.emit_raw(bytes, cx);
    }

    // ----- actions -----

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        match self.selection_text() {
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

    fn add_selection_to_message(
        &mut self,
        _: &AddSelectionToMessage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selection_text() {
            Some(text) if !text.trim().is_empty() => cx.emit(TerminalEvent::AddToMessage(text)),
            // Nothing selected here: the keys may mean something further up.
            _ => cx.propagate(),
        }
    }

    // ----- mouse (called from the element's listeners) -----

    /// Whether a press is the terminal's alone. A right click the program
    /// didn't ask for goes on to the context menu.
    pub(crate) fn keeps_press(&self, event: &MouseDownEvent) -> bool {
        event.button != MouseButton::Right || self.reporting_mouse(&event.modifiers)
    }

    /// Whether the mouse goes to the program: it asked for it, and Shift
    /// (the way to select text inside such programs) isn't held.
    fn reporting_mouse(&self, modifiers: &Modifiers) -> bool {
        !modifiers.shift && self.backend.modes().mouse_tracking
    }

    /// ⌘-click opens a URL under the pointer. Runs before mouse reporting,
    /// so it works inside TUIs too. Returns whether it did.
    fn open_url_at(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        if event.button != MouseButton::Left || !event.modifiers.platform {
            return false;
        }
        let Some(grid) = self.grid else {
            return false;
        };
        let (col, row) = grid.cell_at(event.position);
        match crate::links::url_at(&self.row_text(row as usize), col as usize) {
            Some(url) => {
                cx.emit(TerminalEvent::OpenUrl(url));
                true
            }
            None => false,
        }
    }

    fn report_mouse(
        &mut self,
        grid: Grid,
        action: MouseAction,
        button: Option<TermButton>,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        any_button_pressed: bool,
    ) -> Vec<u8> {
        let (x, y) = grid.device_position(position);
        let report = MouseInput {
            action,
            button,
            mods: input::mods(modifiers, false),
            x,
            y,
            any_button_pressed,
        };
        self.backend.encode_mouse(&report, grid.geometry())
    }

    pub(crate) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        if self.open_url_at(event, cx) {
            return;
        }
        let Some(grid) = self.grid else {
            return;
        };
        if self.reporting_mouse(&event.modifiers) {
            let Some(button) = term_button(event.button) else {
                return;
            };
            if !self.reported_buttons.contains(&button) {
                self.reported_buttons.push(button);
            }
            let bytes = self.report_mouse(
                grid,
                MouseAction::Press,
                Some(button),
                event.position,
                &event.modifiers,
                true,
            );
            self.emit_raw(bytes, cx);
        } else if event.button == MouseButton::Left {
            self.selecting = true;
            self.backend
                .selection_press(grid.grid_point(event.position), gesture_time());
            cx.notify();
        }
    }

    pub(crate) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(grid) = self.grid else {
            return;
        };
        if self.selecting && event.pressed_button != Some(MouseButton::Left) {
            // Released outside the window: we never saw the mouse-up.
            self.selecting = false;
            self.backend.selection_release(None);
            cx.notify();
        }
        if self.selecting {
            let bottom = grid.origin.y + grid.metrics.line_height * f32::from(grid.rows);
            if event.position.y < grid.origin.y {
                self.backend.scroll(Scroll::Delta(-1));
            } else if event.position.y > bottom {
                self.backend.scroll(Scroll::Delta(1));
            }
            self.backend.selection_drag(
                grid.grid_point(event.position),
                grid.geometry(),
                event.modifiers.alt,
            );
            cx.notify();
            return;
        }
        let held = !self.reported_buttons.is_empty();
        if (hovered || held) && self.reporting_mouse(&event.modifiers) {
            let button = self.reported_buttons.first().copied();
            let bytes = self.report_mouse(
                grid,
                MouseAction::Motion,
                button,
                event.position,
                &event.modifiers,
                held,
            );
            self.emit_raw(bytes, cx);
        }
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(grid) = self.grid else {
            return;
        };
        if event.button == MouseButton::Left && self.selecting {
            self.selecting = false;
            self.backend
                .selection_release(Some(grid.grid_point(event.position)));
            cx.notify();
            return;
        }
        let Some(button) = term_button(event.button) else {
            return;
        };
        let Some(index) = self.reported_buttons.iter().position(|b| *b == button) else {
            return;
        };
        self.reported_buttons.remove(index);
        let others = !self.reported_buttons.is_empty();
        let bytes = self.report_mouse(
            grid,
            MouseAction::Release,
            Some(button),
            event.position,
            &event.modifiers,
            others,
        );
        self.emit_raw(bytes, cx);
    }

    pub(crate) fn scroll_wheel(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let Some(grid) = self.grid else {
            return;
        };
        let rows = match event.delta {
            ScrollDelta::Pixels(delta) => delta.y / grid.metrics.line_height,
            ScrollDelta::Lines(delta) => delta.y * WHEEL_ROWS_PER_LINE,
        } + self.wheel_remainder;
        let whole = rows.trunc();
        self.wheel_remainder = rows - whole;
        if whole == 0. {
            return;
        }
        let up = whole > 0.;
        let count = whole.abs() as usize;
        let modes = self.backend.modes();
        if self.reporting_mouse(&event.modifiers) {
            let button = if up {
                TermButton::WheelUp
            } else {
                TermButton::WheelDown
            };
            let held = !self.reported_buttons.is_empty();
            let mut bytes = Vec::new();
            for _ in 0..count {
                bytes.extend(self.report_mouse(
                    grid,
                    MouseAction::Press,
                    Some(button),
                    event.position,
                    &event.modifiers,
                    held,
                ));
            }
            self.emit_raw(bytes, cx);
        } else if modes.alt_screen && modes.alternate_scroll && !event.modifiers.shift {
            // A pager or editor on the alternate screen: the wheel is arrows.
            let arrow = KeyInput {
                key: if up { "up" } else { "down" }.into(),
                action: KeyAction::Press,
                ..KeyInput::default()
            };
            let bytes = self.backend.encode_key(&arrow).repeat(count);
            self.emit_raw(bytes, cx);
        } else {
            let count = isize::try_from(count).unwrap_or(isize::MAX);
            self.backend
                .scroll(Scroll::Delta(if up { -count } else { count }));
            cx.notify();
        }
    }
}

/// The buttons a program can be told about.
fn term_button(button: MouseButton) -> Option<TermButton> {
    match button {
        MouseButton::Left => Some(TermButton::Left),
        MouseButton::Middle => Some(TermButton::Middle),
        MouseButton::Right => Some(TermButton::Right),
        _ => None,
    }
}

/// The terminal's context menu (right click, or Shift-right-click while a
/// program has the mouse). The items are the view's own actions, with
/// their keys shown.
fn context_menu_items(menu: PopupMenu, view: &WeakEntity<TerminalView>, cx: &App) -> PopupMenu {
    let nothing_selected = view
        .upgrade()
        .is_none_or(|view| view.read(cx).selection_text().is_none());
    let menu = menu.menu_with_disabled("Copy", Box::new(Copy), nothing_selected);
    // A page can't read the clipboard on its own: in a browser, pasting is
    // the browser's (no Paste binding there either).
    let menu = if cfg!(target_arch = "wasm32") {
        menu
    } else {
        menu.menu("Paste", Box::new(Paste))
    };
    menu.menu("Select All", Box::new(SelectAll))
        .separator()
        .menu_with_disabled(
            "Add Selection to Message",
            Box::new(AddSelectionToMessage),
            nothing_selected,
        )
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A focus change in the frame that made this view (a pane built and
        // focused while being drawn) happens before its focus subscriptions
        // are live, and they never hear of it: catch up here.
        let focused = window.is_window_active() && self.focus_handle.is_focused(window);
        if focused != self.focused {
            self.focus_changed(focused, cx);
        }
        let colors = theme::colors(cx);
        if self.colors.as_ref() != Some(&colors) {
            self.backend.set_colors(&colors);
            self.colors = Some(colors);
            self.force_full();
        }
        let background = self
            .colors
            .as_ref()
            .map(|c| theme::to_hsla(c.background))
            .unwrap_or_default();
        let weak_view = cx.entity().downgrade();
        div()
            .id("loft-terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::add_selection_to_message))
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .child(TerminalElement::new(cx.entity()))
            .context_menu(move |menu, _, cx| context_menu_items(menu, &weak_view, cx))
    }
}

/// `range` (UTF-16) clamped into a text `len` UTF-16 units long.
fn clamp_utf16(range: &Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    start..range.end.clamp(start, len)
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.marked_text.as_deref()?;
        let clamped = clamp_utf16(&range, text.encode_utf16().count());
        if clamped.is_empty() {
            return None;
        }
        if clamped != range {
            *adjusted_range = Some(clamped.clone());
        }
        let start = input::utf16_to_byte(text, clamped.start);
        let end = input::utf16_to_byte(text, clamped.end);
        Some(text[start..end].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let range = if self.marked_text.is_some() {
            self.marked_selection.clone()
        } else {
            0..0
        };
        Some(UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let text = self.marked_text.as_deref()?;
        Some(0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_selection = 0..0;
        if self.marked_text.take().is_some() {
            cx.notify();
        }
        window.invalidate_character_coordinates();
    }

    /// A paste the platform delivers itself (the browser's paste event):
    /// bracketed when the program asked for that, like the Paste action.
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text()
            && !text.is_empty()
        {
            Self::paste(self, &text, cx);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_composing = self.marked_text.take().is_some();
        self.marked_selection = 0..0;
        let pending = self.pending_key.take();
        if !text.is_empty() {
            let bytes = match pending {
                Some((keystroke, caps_lock)) => {
                    let key = input::committed_text_input(&keystroke, caps_lock, text);
                    let encoded = self.backend.encode_key(&key);
                    if encoded.is_empty() {
                        text.as_bytes().to_vec()
                    } else {
                        encoded
                    }
                }
                None => text.as_bytes().to_vec(),
            };
            self.send_input(bytes, cx);
        } else if was_composing {
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
        // The key that started a composition is never sent itself.
        self.pending_key = None;
        if new_text.is_empty() {
            self.marked_text = None;
            self.marked_selection = 0..0;
        } else {
            let len = new_text.encode_utf16().count();
            self.marked_selection = new_selected_range
                .map(|range| clamp_utf16(&range, len))
                .unwrap_or(len..len);
            self.marked_text = Some(new_text.to_owned());
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
        let x = m.cell_width * (f32::from(col) + range_utf16.start as f32);
        Some(Bounds::new(
            grid.origin + point(x, m.line_height * f32::from(row)),
            size(m.cell_width, m.line_height),
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::px;

    fn grid() -> Grid {
        Grid {
            origin: point(px(10.), px(20.)),
            bounds: Bounds::new(point(px(2.), px(14.)), size(px(200.), px(100.))),
            metrics: CellMetrics {
                cell_width: px(8.),
                line_height: px(16.),
            },
            cols: 10,
            rows: 4,
            scale: 2.,
        }
    }

    #[test]
    fn pointer_maps_to_cells_and_device_pixels() {
        let g = grid();
        assert_eq!(
            g.geometry(),
            Geometry {
                cols: 10,
                rows: 4,
                cell_width: 16,
                cell_height: 32
            }
        );
        assert_eq!(g.cell_at(point(px(10.), px(20.))), (0, 0));
        assert_eq!(g.cell_at(point(px(29.), px(53.))), (2, 2));
        // Outside the grid: the nearest edge cell.
        assert_eq!(g.cell_at(point(px(0.), px(0.))), (0, 0));
        assert_eq!(g.cell_at(point(px(500.), px(500.))), (9, 3));
        // Centre of cell (2, 3) in device pixels.
        let (x, y) = g.device_position(point(px(10. + 20.), px(20. + 56.)));
        assert_eq!((x, y), (40., 112.));
        assert_eq!(g.device_position(point(px(0.), px(0.))), (0., 0.));
        assert_eq!(g.device_position(point(px(900.), px(900.))), (159., 127.));
        let p = g.grid_point(point(px(30.), px(76.)));
        assert_eq!((p.col, p.row, p.x, p.y), (2, 3, 40., 112.));
    }

    #[test]
    fn utf16_ranges_clamp_into_the_composition() {
        assert_eq!(clamp_utf16(&(1..9), 4), 1..4);
        assert_eq!(clamp_utf16(&(7..9), 4), 4..4);
        let reversed = Range { start: 3, end: 1 };
        assert_eq!(clamp_utf16(&reversed, 4), 3..3);
    }
}
