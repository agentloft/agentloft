//! A page of the daemon's own browser (ADR-0017), shown as the pictures the
//! daemon streams and driven by sending the viewer's mouse and keys back.
//! This is the browser tab of the web client, where there is no native web
//! view to embed; nothing in it is specific to the web.

use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Bounds, ClipboardItem, Context, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, RenderImage, ScrollEvent,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, UTF16Selection, Window,
    canvas, div, img, px,
};
use loft_protocol::{
    ItemId, PageAction, PageButton, PageInfo, PageInput, PageMouse, PageNav, Request, Response,
    ThreadId, ViewTheme, page_modifiers,
};
use std::cell::Cell;
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

gpui_kit::actions!(page_view, [CopySelection, CutSelection, PasteClipboard]);

const KEY_CONTEXT: &str = "PageView";
const TOOLBAR_HEIGHT: f32 = 40.;
/// Mouse moves are sent at most this often.
const MOVE_EVERY: Duration = Duration::from_millis(16);
/// A resize is sent once the size has held this long.
const RESIZE_AFTER: Duration = Duration::from_millis(120);
/// How tall a view is drawn before its page has said how tall it is.
const VIEW_FIRST_HEIGHT: u32 = 120;
/// A view that hasn't been on screen for this long lets go of its page
/// (the daemon closes pages nobody watches; it comes back when seen again).
const VIEW_UNSEEN: Duration = Duration::from_secs(20);

/// A view's page changed height: whatever holds it should measure again.
pub struct PageResized;
impl gpui_kit::EventEmitter<PageResized> for PageView {}

struct Initialized;
impl gpui_kit::Global for Initialized {}

thread_local! {
    /// How many frames each window has drawn, by window id.
    static FRAMES: std::cell::RefCell<std::collections::HashMap<u64, u64>> =
        std::cell::RefCell::default();
}

/// A window is drawing a frame: its root view says so, once per frame.
///
/// A view's page is let go when it is out of sight, and "not painted for a
/// while" doesn't say that: a window where nothing changes paints nothing,
/// with the view in the middle of it. What says it is the window drawing
/// frames the view is not in.
pub fn frame_drawn(window: &Window) {
    let id = window.window_handle().window_id().as_u64();
    FRAMES.with(|frames| *frames.borrow_mut().entry(id).or_default() += 1);
}

/// The frame a view's page was last painted in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Seen {
    window: u64,
    frame: u64,
}

impl Seen {
    pub(crate) fn now(window: &Window) -> Self {
        let window = window.window_handle().window_id().as_u64();
        Self {
            window,
            frame: Self::latest(window),
        }
    }

    fn latest(window: u64) -> u64 {
        FRAMES.with(|frames| frames.borrow().get(&window).copied().unwrap_or(0))
    }

    /// Whether that is still the last frame its window drew: the view is on
    /// screen, however long ago that was.
    pub(crate) fn in_sight(self) -> bool {
        Self::latest(self.window) == self.frame
    }
}

/// The pointer for what CSS calls `cursor` (what a page asked for where the
/// mouse is); the arrow for anything unknown.
pub(crate) fn pointer_for(cursor: &str) -> gpui_kit::CursorStyle {
    use gpui_kit::CursorStyle as C;
    match cursor {
        "pointer" => C::PointingHand,
        "text" => C::IBeam,
        "vertical-text" => C::IBeamCursorForVerticalLayout,
        "crosshair" => C::Crosshair,
        "grab" | "move" | "all-scroll" => C::OpenHand,
        "grabbing" => C::ClosedHand,
        "not-allowed" | "no-drop" => C::OperationNotAllowed,
        "copy" => C::DragCopy,
        "alias" => C::DragLink,
        "context-menu" => C::ContextualMenu,
        "col-resize" => C::ResizeColumn,
        "row-resize" => C::ResizeRow,
        "ew-resize" => C::ResizeLeftRight,
        "ns-resize" => C::ResizeUpDown,
        "e-resize" => C::ResizeRight,
        "w-resize" => C::ResizeLeft,
        "n-resize" => C::ResizeUp,
        "s-resize" => C::ResizeDown,
        "nwse-resize" | "nw-resize" | "se-resize" => C::ResizeUpLeftDownRight,
        "nesw-resize" | "ne-resize" | "sw-resize" => C::ResizeUpRightDownLeft,
        _ => C::Arrow,
    }
}

/// Carry out what a view asked for on its viewer's click. May be called
/// while the store is handing out what the daemon sent, so anything that
/// needs the store waits until it is done.
pub(crate) fn act(store: &Entity<AppStore>, thread: &ThreadId, action: PageAction, cx: &mut App) {
    let (store, thread) = (store.clone(), thread.clone());
    match action {
        PageAction::Send { text } => {
            let text = text.trim().to_owned();
            if !text.is_empty() {
                cx.defer(move |cx| store.update(cx, |store, cx| store.say(thread, text, cx)));
            }
        }
        PageAction::Copy { text } => cx.write_to_clipboard(ClipboardItem::new_string(text)),
        PageAction::Open { target } => {
            let target = target.trim().to_owned();
            if target.starts_with("http://") || target.starts_with("https://") {
                cx.open_url(&target);
            } else if !target.is_empty() && !target.contains("://") {
                // A file of the thread's folder, shown in the panel.
                cx.defer(move |cx| {
                    let cwd = store.read(cx).thread_cwd(&thread, cx).unwrap_or_default();
                    let path = target
                        .split_once(':')
                        .map_or(target.as_str(), |(path, _line)| path);
                    let rel = crate::items::relative_to(path, &cwd);
                    store.update(cx, |store, cx| store.open_file(thread, rel, cx));
                });
            }
        }
    }
}

/// Whether a request failed because the connection to the daemon ended,
/// rather than because of what was asked.
fn lost_connection(err: &loft_protocol::ErrorBody) -> bool {
    err.code == loft_protocol::ErrorCode::Internal && err.message.ends_with("agentloftd")
}

fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    let ctx = Some(KEY_CONTEXT);
    // Copy and cut go through the viewer's own clipboard. Paste has a key
    // only where the app can read the clipboard itself: in a browser, the
    // browser's own paste brings the text (`EntityInputHandler::paste`).
    let mut bindings = vec![
        KeyBinding::new("cmd-c", CopySelection, ctx),
        KeyBinding::new("ctrl-c", CopySelection, ctx),
        KeyBinding::new("cmd-x", CutSelection, ctx),
        KeyBinding::new("ctrl-x", CutSelection, ctx),
    ];
    if !crate::host::IS_WEB {
        bindings.push(KeyBinding::new("cmd-v", PasteClipboard, ctx));
        bindings.push(KeyBinding::new("ctrl-v", PasteClipboard, ctx));
    }
    cx.bind_keys(bindings);
}

pub struct PageView {
    store: Entity<AppStore>,
    thread: ThreadId,
    focus: FocusHandle,
    address: Entity<InputState>,
    editing_address: bool,
    /// The address to show once the field isn't being typed in.
    pending_address: Option<String>,
    info: Option<PageInfo>,
    /// Where the page last was, to return there if it has to be reopened
    /// (the daemon restarted, or its browser stopped).
    last_url: Option<String>,
    opening: bool,
    /// Why there is no page (it closed, or wouldn't open).
    note: Option<String>,
    /// The page as last pictured, and the subscription it came on.
    picture: Option<Arc<RenderImage>>,
    /// A picture is being decoded; the newest to arrive meanwhile waits.
    decoding: bool,
    waiting: Option<(u64, u64, Vec<u8>)>,
    /// Where the page is drawn (set while painting).
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The size last asked of the daemon, and the scale it was asked at.
    asked: Option<(u32, u32, f32)>,
    resize: Option<Task<()>>,
    held: PageButton,
    /// Keys sent down and not yet up, by `code`.
    keys_down: HashSet<String>,
    /// The keystroke whose text the platform is about to commit.
    text_key: Option<Keystroke>,
    marked: Option<String>,
    last_move: Option<Instant>,
    last_point: Point<Pixels>,
    /// Set when this draws a view an agent made (the `show_ui` call's
    /// item, ADR-0018) rather than a browser tab: no toolbar, as tall as
    /// its content, and it only scrolls or takes keys once clicked.
    view: Option<ItemId>,
    /// The look the daemon was last told (a view dresses like the app).
    themed: Option<ViewTheme>,
    /// The height a view's page last had: what it keeps while reopening.
    last_height: Option<u32>,
    /// The frame of its window the page was last painted in.
    seen: Rc<Cell<Option<Seen>>>,
    /// The newest picture not drawn yet, and when it arrived (for the log).
    arrived: Rc<Cell<Option<(u64, Instant)>>>,
    /// The page couldn't be opened because there was no daemon to ask (it
    /// was restarting): it is opened as soon as there is one again.
    offline: bool,
    /// Lets go of a view's page once it has gone unseen.
    resting: Option<Task<()>>,
    /// Whether a view's page is the top of its card (no title over it),
    /// so its upper corners are the card's.
    flush_top: Rc<Cell<bool>>,
    _subs: Vec<Subscription>,
}

impl Focusable for PageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PageView {
    pub fn new(
        store: Entity<AppStore>,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        init(cx);
        let address =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter an address"));
        // A new tab opens the home page, if one is set (typed as one would
        // type it in the address bar: "example.com" is an address).
        let home = {
            let browser = &store.read(cx).browser;
            browser.home().and_then(|home| {
                loft_browser::normalize_address(home, &browser.search_template())
                    .ok()
                    .map(|url| url.to_string())
            })
        };
        let subs = vec![
            cx.subscribe_in(&address, window, Self::on_address_event),
            // Back in touch with the daemon: a page that was waiting for
            // that opens now, with nobody having to touch it.
            cx.observe(&store, |this, store, cx| {
                if this.offline && store.read(cx).client().is_some() {
                    this.offline = false;
                    cx.notify();
                }
            }),
            // The view going away (its thread's panel closed) closes the
            // page: nothing else would.
            cx.on_release(|this, cx| {
                // A view's page is the daemon's to close (others may be
                // watching it, and it is cheap to keep for a while).
                let closes = this.view.is_none();
                if let Some(info) = this.info.take() {
                    this.store.update(cx, |store, _| {
                        store.forget_page(&info.id);
                        if closes && let Some(client) = store.client() {
                            drop(client.request(Request::PageClose { id: info.id }));
                        }
                    });
                }
                if let Some(picture) = this.picture.take() {
                    cx.drop_image(picture, None);
                }
            }),
        ];
        Self {
            store,
            thread,
            focus: cx.focus_handle(),
            address,
            editing_address: false,
            pending_address: None,
            info: None,
            last_url: home,
            opening: false,
            note: None,
            picture: None,
            decoding: false,
            waiting: None,
            bounds: Rc::default(),
            asked: None,
            resize: None,
            held: PageButton::None,
            keys_down: HashSet::new(),
            text_key: None,
            marked: None,
            last_move: None,
            last_point: Point::default(),
            view: None,
            themed: None,
            last_height: None,
            seen: Rc::default(),
            arrived: Rc::default(),
            offline: false,
            resting: None,
            flush_top: Rc::default(),
            _subs: subs,
        }
    }

    /// The page of a view an agent made: the `show_ui` call `item`.
    pub fn for_view(
        store: Entity<AppStore>,
        thread: ThreadId,
        item: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(store, thread, window, cx);
        this.view = Some(item);
        this.last_url = None;
        this
    }

    /// Whether a view's page is the top of its card. Whoever draws the
    /// card says so each time it draws.
    /// Where the page is drawn in its window, once it has been.
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds.get()
    }

    pub fn flush_top(&self) -> Rc<Cell<bool>> {
        self.flush_top.clone()
    }

    pub fn title(&self) -> Option<&str> {
        self.info.as_ref().map(|i| i.title.as_str())
    }

    pub fn url(&self) -> Option<&str> {
        self.info.as_ref().map(|i| i.url.as_str())
    }

    pub fn is_loading(&self) -> bool {
        self.opening || self.info.as_ref().is_some_and(|i| i.loading)
    }

    /// Whether the tab opens on nothing (no home page to go to).
    pub fn opens_blank(&self) -> bool {
        self.last_url.is_none()
    }

    pub fn focus_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Send the page to `url` (a link someone clicked elsewhere in the app).
    pub fn go(&mut self, url: &str, cx: &mut Context<Self>) {
        if self.info.is_some() {
            self.navigate(
                PageNav::Url {
                    url: url.to_owned(),
                },
                cx,
            );
        } else {
            // Not open yet: it opens there.
            self.last_url = Some(url.to_owned());
        }
    }

    /// Close the page for good (its tab is going away).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = self.info.take() {
            self.store.update(cx, |s, _| s.forget_page(&info.id));
            self.send(Request::PageClose { id: info.id }, cx);
        }
        self.drop_picture(cx);
    }

    // -- the daemon's side ----------------------------------------------------

    fn send(&self, body: Request, cx: &mut Context<Self>) {
        if let Some(client) = self.store.read(cx).client() {
            let sent = client.request(body);
            cx.background_executor()
                .spawn(async move {
                    let _ = sent.await;
                })
                .detach();
        }
    }

    fn input(&self, input: PageInput, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            self.send(
                Request::PageInput {
                    id: info.id.clone(),
                    input,
                },
                cx,
            );
        }
    }

    fn navigate(&mut self, nav: PageNav, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let body = Request::PageNavigate {
            id: info.id.clone(),
            nav,
        };
        request_in(&self.store, body, cx, |this, result, cx| {
            if let Err(err) = result {
                this.store.update(cx, |_, cx| {
                    cx.emit(crate::store::StoreEvent::Notify {
                        message: err.message,
                        error: true,
                    })
                });
            }
        });
    }

    /// The size the page should have: the area it is drawn in.
    fn wanted_size(&self, window: &Window) -> Option<(u32, u32, f32)> {
        let bounds = self.bounds.get()?;
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        // A view's width is ours to say; its height is its content's.
        if self.view.is_some() {
            return (w >= 1.).then(|| (w.round() as u32, 0, window.scale_factor().clamp(1., 2.)));
        }
        (w >= 1. && h >= 1.).then(|| {
            (
                w.round() as u32,
                h.round() as u32,
                window.scale_factor().clamp(1., 2.),
            )
        })
    }

    fn open(&mut self, size: (u32, u32, f32), cx: &mut Context<Self>) {
        self.opening = true;
        self.note = None;
        self.asked = Some(size);
        let body = match &self.view {
            Some(item) => {
                let theme = crate::view::theme(cx);
                self.themed = Some(theme.clone());
                Request::ViewOpen {
                    thread_id: self.thread.clone(),
                    item_id: item.clone(),
                    width: size.0,
                    scale: size.2,
                    theme: Box::new(theme),
                }
            }
            None => Request::PageOpen {
                thread_id: self.thread.clone(),
                url: self.last_url.clone(),
                width: size.0,
                height: size.1,
                scale: size.2,
            },
        };
        request_in(&self.store, body, cx, |this, result, cx| {
            this.opening = false;
            match result {
                Ok(Response::Page { page }) => {
                    let weak = cx.entity().downgrade();
                    this.store
                        .update(cx, |s, _| s.subscribe_page(page.id.clone(), weak));
                    this.on_state(page, cx);
                    if this.view.is_some() {
                        this.rest_when_unseen(cx);
                    }
                }
                Ok(other) => this.note = Some(format!("unexpected response: {other:?}")),
                // The daemon is away (restarting, being replaced by a newer
                // build): that is not the page failing, and nothing for the
                // viewer to retry. It opens when the daemon is back.
                Err(_) if this.store.read(cx).client().is_none() => this.offline = true,
                // Asked on a connection that ended meanwhile: the redraw
                // below asks again on the one there is now.
                Err(err) if lost_connection(&err) => {}
                Err(err) => this.note = Some(err.message),
            }
            cx.notify();
        });
        cx.notify();
    }

    /// Open the page once its size is known, and keep the daemon's page the
    /// size of this view.
    fn follow_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(size) = self.wanted_size(window) else {
            return;
        };
        if self.info.is_none() {
            if !self.opening && !self.offline && self.note.is_none() {
                self.open(size, cx);
            }
            return;
        }
        if self.asked == Some(size) {
            return;
        }
        self.asked = Some(size);
        // Dragging a divider changes the size every frame: send the last.
        self.resize = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                if this.asked == Some(size)
                    && let Some(info) = &this.info
                {
                    this.send(
                        Request::PageResize {
                            id: info.id.clone(),
                            width: size.0,
                            height: size.1,
                            scale: size.2,
                        },
                        cx,
                    );
                }
            });
        }));
    }

    // -- fed by the store -------------------------------------------------------

    pub fn on_state(&mut self, page: PageInfo, cx: &mut Context<Self>) {
        if self.info.as_ref().is_none_or(|old| old.url != page.url) {
            self.pending_address = Some(page.url.clone());
        }
        if !page.url.is_empty() {
            self.last_url = Some(page.url.clone());
        }
        if self.view.is_some() && self.last_height != Some(page.height) {
            self.last_height = Some(page.height);
            cx.emit(PageResized);
        }
        if self
            .info
            .as_ref()
            .is_some_and(|old| old.cursor != page.cursor)
        {
            tracing::debug!(cursor = %page.cursor, "the page wants another pointer");
        }
        self.info = Some(page);
        self.note = None;
        cx.notify();
    }

    /// What a view asked for on this viewer's click. Called while the
    /// store is handing out what the daemon sent, so anything that needs
    /// the store waits until it is done.
    pub fn on_action(&mut self, action: PageAction, cx: &mut Context<Self>) {
        act(&self.store, &self.thread, action, cx);
    }

    /// The look a view should have, if it changed since the daemon was
    /// told (light to dark, another accent).
    fn follow_theme(&mut self, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let theme = crate::view::theme(cx);
        if self.themed.as_ref() != Some(&theme) {
            self.send(
                Request::ViewTheme {
                    id: info.id.clone(),
                    theme: Box::new(theme.clone()),
                },
                cx,
            );
            self.themed = Some(theme);
        }
    }

    /// Check now and then whether the view is still drawn; when it hasn't
    /// been for a while, stop watching its page. Its last picture stays,
    /// and drawing it again opens the page again.
    fn rest_when_unseen(&mut self, cx: &mut Context<Self>) {
        self.resting = Some(cx.spawn(async move |this, cx| {
            // Since when the window has been drawing without the view.
            let mut gone: Option<Instant> = None;
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let resting = this.update(cx, |this, cx| {
                    let Some(info) = &this.info else {
                        return true;
                    };
                    if this.seen.get().is_some_and(Seen::in_sight) {
                        gone = None;
                        return false;
                    }
                    if gone.get_or_insert_with(Instant::now).elapsed() < VIEW_UNSEEN {
                        return false;
                    }
                    let id = info.id.clone();
                    this.store.update(cx, |store, _| store.forget_page(&id));
                    this.info = None;
                    this.asked = None;
                    this.held = PageButton::None;
                    this.keys_down.clear();
                    true
                });
                if resting.unwrap_or(true) {
                    break;
                }
            }
        }));
    }

    /// A new picture. One is decoded at a time; while that happens only the
    /// newest to arrive is kept, and the daemon sends no more until this
    /// one is acknowledged.
    pub fn on_frame(&mut self, sub_id: u64, seq: u64, jpeg: Vec<u8>, cx: &mut Context<Self>) {
        if self.decoding {
            if let Some((old_sub, old_seq, _)) = self.waiting.replace((sub_id, seq, jpeg)) {
                self.ack(old_sub, old_seq, cx);
            }
            return;
        }
        self.decoding = true;
        let (arrived, bytes) = (Instant::now(), jpeg.len());
        let decoded = crate::host::decode_jpeg(jpeg, cx);
        cx.spawn(async move |this, cx| {
            let picture = decoded.await;
            let _ = this.update(cx, |this, cx| {
                this.decoding = false;
                if let Some(picture) = picture {
                    if this.view.is_some() && this.picture.is_none() {
                        tracing::debug!(seq, "a view's first picture is in");
                    }
                    tracing::trace!(
                        seq,
                        bytes,
                        ms = arrived.elapsed().as_millis() as u64,
                        "a page's picture is decoded"
                    );
                    this.arrived.set(Some((seq, arrived)));
                    this.drop_picture(cx);
                    this.picture = Some(picture);
                    cx.notify();
                }
                this.ack(sub_id, seq, cx);
                if let Some((sub_id, seq, jpeg)) = this.waiting.take() {
                    this.on_frame(sub_id, seq, jpeg, cx);
                }
            });
        })
        .detach();
    }

    fn ack(&self, sub_id: u64, seq: u64, cx: &mut Context<Self>) {
        if let Some(client) = self.store.read(cx).client() {
            client.ack_page_frame(sub_id, seq);
        }
    }

    /// Free the old picture's texture; each one is a new image to GPUI.
    fn drop_picture(&mut self, cx: &mut Context<Self>) {
        if let Some(old) = self.picture.take() {
            cx.drop_image(old, None);
        }
    }

    pub fn on_closed(&mut self, reason: String, cx: &mut Context<Self>) {
        self.info = None;
        self.asked = None;
        self.held = PageButton::None;
        self.keys_down.clear();
        // A view's page comes and goes (the daemon closes idle ones); it
        // opens again the next time the view is drawn.
        if self.view.is_none() {
            self.note = Some(reason);
        }
        cx.notify();
    }

    /// The daemon restarted and its browser with it: open a new page. Called
    /// while the store is updating, so the reopen runs on the next tick.
    pub fn on_lost(&mut self, cx: &mut Context<Self>) {
        self.info = None;
        self.asked = None;
        self.note = None;
        self.offline = false;
        cx.notify();
    }

    // -- the address bar -------------------------------------------------------

    fn on_address_event(
        &mut self,
        state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                let typed = state.read(cx).value().to_string();
                self.editing_address = false;
                if typed.trim().is_empty() {
                    return;
                }
                let search = self.store.read(cx).browser.search_template();
                match loft_browser::normalize_address(&typed, &search) {
                    Ok(url) => self.navigate(
                        PageNav::Url {
                            url: url.to_string(),
                        },
                        cx,
                    ),
                    Err(err) => self.store.update(cx, |_, cx| {
                        cx.emit(crate::store::StoreEvent::Notify {
                            message: err.to_string(),
                            error: true,
                        })
                    }),
                }
                // Once this key press is over: focused now, the page would
                // get the Enter that sent it here.
                let focus = self.focus.clone();
                window.on_next_frame(move |window, cx| window.focus(&focus, cx));
            }
            InputEvent::Focus => {
                self.editing_address = true;
                state.update(cx, |state, cx| state.select_all(window, cx));
            }
            InputEvent::Blur => {
                self.editing_address = false;
                cx.notify();
            }
            InputEvent::Change => {}
        }
    }

    // -- mouse -----------------------------------------------------------------

    /// A window position as a point in the page, if the page is showing.
    fn page_point(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.bounds.get()?;
        let info = self.info.as_ref()?;
        let (x, y) = (
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        );
        // A view's picture is stretched over its element while the page
        // catches up with a new size.
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if self.view.is_some() && w >= 1. && h >= 1. {
            return Some((x * info.width as f32 / w, y * info.height as f32 / h));
        }
        Some((x, y))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        let Some((x, y)) = self.page_point(event.position) else {
            // No page behind the picture (it was let go, or the daemon
            // restarted): a click is as good a moment as any to get one.
            tracing::debug!(
                opening = self.opening,
                offline = self.offline,
                "a press with no page to go to"
            );
            cx.notify();
            return;
        };
        self.last_point = event.position;
        self.held = button(event.button);
        tracing::debug!(
            x,
            y,
            at = ?event.position,
            bounds = ?self.bounds.get(),
            page = ?self.info.as_ref().map(|i| (i.width, i.height)),
            count = event.click_count,
            first = event.first_mouse,
            "a press goes to the page"
        );
        self.input(
            PageInput::Mouse {
                action: PageMouse::Down,
                x,
                y,
                button: self.held,
                click_count: event.click_count.min(3) as u8,
                modifiers: modifiers(&event.modifiers),
            },
            cx,
        );
        cx.stop_propagation();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((x, y)) = self.page_point(event.position) else {
            tracing::debug!("a release with no page to go to");
            return;
        };
        tracing::debug!(
            x,
            y,
            held = ?self.held,
            count = event.click_count,
            "a release goes to the page"
        );
        self.held = PageButton::None;
        self.input(
            PageInput::Mouse {
                action: PageMouse::Up,
                x,
                y,
                button: button(event.button),
                click_count: event.click_count.min(3) as u8,
                modifiers: modifiers(&event.modifiers),
            },
            cx,
        );
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self
            .last_move
            .is_some_and(|at| now.duration_since(at) < MOVE_EVERY)
        {
            return;
        }
        let Some((x, y)) = self.page_point(event.position) else {
            return;
        };
        self.last_move = Some(now);
        // A button let go outside the page never reached us.
        if event.pressed_button.is_none() {
            self.held = PageButton::None;
        }
        self.input(
            PageInput::Mouse {
                action: PageMouse::Move,
                x,
                y,
                button: self.held,
                click_count: 0,
                modifiers: modifiers(&event.modifiers),
            },
            cx,
        );
    }

    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A view scrolls with the thread around it. Only one with more
        // content than it can show, and only once clicked into, takes the
        // wheel for itself.
        if self.view.is_some()
            && !(self.info.as_ref().is_some_and(|i| i.overflow) && self.focus.is_focused(window))
        {
            return;
        }
        let Some((x, y)) = self.page_point(event.position) else {
            return;
        };
        let delta = event.delta.pixel_delta(window.line_height());
        self.input(
            PageInput::Wheel {
                x,
                y,
                dx: -f32::from(delta.x),
                dy: -f32::from(delta.y),
                modifiers: modifiers(&event.modifiers),
            },
            cx,
        );
        cx.stop_propagation();
    }

    // -- keys ------------------------------------------------------------------

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_some() || self.info.is_none() {
            return;
        }
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        self.text_key = None;
        // The browser's own paste (see `init`): let it through.
        if crate::host::IS_WEB && (m.platform || m.control) && keystroke.key == "v" {
            return;
        }
        let types_text = keystroke.key_char.as_deref().is_some_and(is_text)
            && !m.platform
            && !m.control
            && keystroke.key != "enter"
            && keystroke.key != "tab";
        if types_text && !event.is_held {
            // The platform's text input commits it next (so input methods
            // and dead keys work); the key goes with the text then.
            self.text_key = Some(keystroke.clone());
            return;
        }
        let (key, code) = dom_key(keystroke);
        self.keys_down.insert(code.clone());
        self.input(
            PageInput::Key {
                down: true,
                text: types_text.then(|| keystroke.key_char.clone()).flatten(),
                key,
                code,
                modifiers: modifiers(m),
                repeat: event.is_held,
            },
            cx,
        );
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (key, code) = dom_key(&event.keystroke);
        if !self.keys_down.remove(&code) {
            return;
        }
        self.input(
            PageInput::Key {
                down: false,
                text: None,
                key,
                code,
                modifiers: modifiers(&event.keystroke.modifiers),
                repeat: false,
            },
            cx,
        );
    }

    /// Text the platform committed: one typed character goes as its key
    /// (pages listen for keys), anything else as text.
    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let key = self.text_key.take();
        if text.is_empty() {
            return;
        }
        match key {
            Some(keystroke) if keystroke.key_char.as_deref() == Some(text) => {
                let (key, code) = dom_key(&keystroke);
                let modifiers = modifiers(&keystroke.modifiers);
                for down in [true, false] {
                    self.input(
                        PageInput::Key {
                            down,
                            text: down.then(|| text.to_owned()),
                            key: key.clone(),
                            code: code.clone(),
                            modifiers,
                            repeat: false,
                        },
                        cx,
                    );
                }
            }
            _ => self.input(
                PageInput::Text {
                    text: text.to_owned(),
                },
                cx,
            ),
        }
    }

    /// Copy (or cut) what is selected in the page to this device's
    /// clipboard: the page's own clipboard is the daemon machine's.
    fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let body = Request::PageSelection {
            id: info.id.clone(),
        };
        request_in(&self.store, body, cx, move |this, result, cx| {
            if let Ok(Response::Text { text }) = result
                && !text.is_empty()
            {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                if cut {
                    for down in [true, false] {
                        this.input(
                            PageInput::Key {
                                down,
                                text: None,
                                key: "Backspace".into(),
                                code: "Backspace".into(),
                                modifiers: 0,
                                repeat: false,
                            },
                            cx,
                        );
                    }
                }
            }
        });
    }

    fn paste_clipboard(&mut self, _: &PasteClipboard, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.input(PageInput::Text { text }, cx);
        }
    }
}

/// Text a key types (not a control character).
fn is_text(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_control)
}

fn button(button: MouseButton) -> PageButton {
    match button {
        MouseButton::Left => PageButton::Left,
        MouseButton::Right => PageButton::Right,
        MouseButton::Middle => PageButton::Middle,
        MouseButton::Navigate(_) => PageButton::None,
    }
}

fn modifiers(m: &Modifiers) -> u8 {
    let mut bits = 0;
    if m.alt {
        bits |= page_modifiers::ALT;
    }
    if m.control {
        bits |= page_modifiers::CONTROL;
    }
    if m.platform {
        bits |= page_modifiers::META;
    }
    if m.shift {
        bits |= page_modifiers::SHIFT;
    }
    bits
}

/// A GPUI keystroke as a DOM `KeyboardEvent`'s `key` and `code`.
fn dom_key(keystroke: &Keystroke) -> (String, String) {
    let key = keystroke.key.as_str();
    let named = |name: &str| (name.to_owned(), name.to_owned());
    match key {
        "enter" => named("Enter"),
        "tab" => named("Tab"),
        "escape" => named("Escape"),
        "backspace" => named("Backspace"),
        "delete" => named("Delete"),
        "insert" => named("Insert"),
        "up" => named("ArrowUp"),
        "down" => named("ArrowDown"),
        "left" => named("ArrowLeft"),
        "right" => named("ArrowRight"),
        "home" => named("Home"),
        "end" => named("End"),
        "pageup" => named("PageUp"),
        "pagedown" => named("PageDown"),
        "space" => (" ".into(), "Space".into()),
        _ => {
            if let Some(n) = key.strip_prefix('f')
                && n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n))
            {
                return named(&format!("F{n}"));
            }
            let typed = keystroke
                .key_char
                .clone()
                .filter(|c| is_text(c))
                .unwrap_or_else(|| {
                    if keystroke.modifiers.shift {
                        key.to_uppercase()
                    } else {
                        key.to_owned()
                    }
                });
            let mut chars = key.chars();
            let code = match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    format!("Key{}", c.to_ascii_uppercase())
                }
                (Some(c), None) if c.is_ascii_digit() => format!("Digit{c}"),
                (Some(c), None) => match c {
                    '-' => "Minus",
                    '=' => "Equal",
                    '[' => "BracketLeft",
                    ']' => "BracketRight",
                    '\\' => "Backslash",
                    ';' => "Semicolon",
                    '\'' => "Quote",
                    ',' => "Comma",
                    '.' => "Period",
                    '/' => "Slash",
                    '`' => "Backquote",
                    _ => "",
                }
                .to_owned(),
                _ => String::new(),
            };
            (typed, code)
        }
    }
}

impl EntityInputHandler for PageView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.marked.clone()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self
            .marked
            .as_ref()
            .map_or(0, |text| text.encode_utf16().count());
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    /// A paste the platform delivers itself (the browser's paste event).
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text()
            && !text.is_empty()
        {
            self.input(PageInput::Text { text }, cx);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.commit_text(text, cx);
    }

    /// What an input method is composing. It isn't shown in the page; the
    /// text arrives there when it is committed.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.text_key = None;
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
    }

    /// Input-method windows open by where the page was last clicked.
    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            self.last_point,
            gpui_kit::size(px(1.), px(18.)),
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

impl PageView {
    /// A view an agent made: its page and nothing around it.
    fn render_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = Palette::of(cx);
        self.follow_size(window, cx);
        self.follow_theme(cx);
        let height = self
            .info
            .as_ref()
            .map(|i| i.height)
            .or(self.last_height)
            .unwrap_or(VIEW_FIRST_HEIGHT);
        let (bounds, seen, focus, entity) = (
            self.bounds.clone(),
            self.seen.clone(),
            self.focus.clone(),
            cx.entity(),
        );
        let arrived = self.arrived.clone();
        let id = entity.entity_id();
        let body = match (&self.note, &self.picture) {
            (Some(note), _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(crate::host_ui::unavailable(
                    IconName::Globe,
                    "This view can't be shown",
                    note,
                    &p,
                ))
                .child(
                    Button::new("view-retry")
                        .small()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.note = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
            // The card's corners are round; a picture's are not, unless
            // told so.
            (None, Some(picture)) => img(picture.clone())
                .size_full()
                .rounded_b(px(11.))
                .when(self.flush_top.get(), |i| i.rounded_t(px(11.)))
                .into_any_element(),
            (None, None) => h_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(Spinner::new().xsmall().color(p.muted_foreground))
                .child("Drawing…")
                .into_any_element(),
        };
        let height = if self.note.is_some() { 180 } else { height };
        div()
            .id(("view-page", id))
            .relative()
            .w_full()
            .h(px(height as f32))
            .overflow_hidden()
            // The pointer the page wants here: it is a picture to us.
            .cursor(pointer_for(
                self.info.as_ref().map_or("", |info| info.cursor.as_str()),
            ))
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy_selection(false, cx)))
            .on_action(cx.listener(|this, _: &CutSelection, _, cx| this.copy_selection(true, cx)))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll(cx.listener(Self::on_scroll))
            // The pointer left: the page shouldn't go on showing a hover.
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.held == PageButton::None {
                    this.input(
                        PageInput::Mouse {
                            action: PageMouse::Move,
                            x: -10.,
                            y: -10.,
                            button: PageButton::None,
                            click_count: 0,
                            modifiers: 0,
                        },
                        cx,
                    );
                }
            }))
            .child(body)
            .child(
                canvas(
                    move |area, window, cx| {
                        seen.set(Some(Seen::now(window)));
                        if let Some((seq, at)) = arrived.take() {
                            tracing::trace!(
                                seq,
                                ms = at.elapsed().as_millis() as u64,
                                "a page's picture is drawn"
                            );
                        }
                        // Its width is known from here on: open, or resize.
                        let before = bounds.replace(Some(area));
                        if before.is_none_or(|b| b.size.width != area.size.width) {
                            cx.notify(id);
                        }
                    },
                    move |area, (), window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(area, entity), cx);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }
}

impl Render for PageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.view.is_some() {
            return self.render_view(window, cx);
        }
        let p = Palette::of(cx);
        // A browser session the daemon doesn't give its browser to.
        if !self.store.read(cx).pages_allowed() {
            return crate::host_ui::unavailable(
                IconName::Globe,
                "The browser is off for web access",
                "Turn it on in the desktop app: Settings ▸ Web access.",
                &p,
            );
        }
        self.follow_size(window, cx);
        if !self.editing_address
            && let Some(url) = self.pending_address.take()
        {
            self.address
                .update(cx, |state, cx| state.set_value(url, window, cx));
        }
        let info = self.info.clone();
        let loading = self.is_loading();
        let ready = info.is_some();
        let prefix = if loading {
            Spinner::new()
                .xsmall()
                .color(p.muted_foreground)
                .into_any_element()
        } else {
            Icon::new(IconName::Globe)
                .xsmall()
                .text_color(p.muted_foreground)
                .into_any_element()
        };
        let toolbar = h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .child(
                Button::new("page-back")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::ArrowLeft)
                    .disabled(!info.as_ref().is_some_and(|i| i.can_go_back))
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(PageNav::Back, cx))),
            )
            .child(
                Button::new("page-forward")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::ArrowRight)
                    .disabled(!info.as_ref().is_some_and(|i| i.can_go_forward))
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(PageNav::Forward, cx))),
            )
            .child(
                Button::new("page-reload")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(if loading {
                        IconName::Close
                    } else {
                        IconName::RotateCw
                    })
                    .disabled(!ready)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let nav = if this.info.as_ref().is_some_and(|i| i.loading) {
                            PageNav::Stop
                        } else {
                            PageNav::Reload
                        };
                        this.navigate(nav, cx);
                    })),
            )
            .child(
                div().flex_1().min_w_0().pl_1().child(
                    Input::new(&self.address)
                        .small()
                        .prefix(prefix)
                        .disabled(!ready),
                ),
            );

        let (bounds, focus, entity) = (self.bounds.clone(), self.focus.clone(), cx.entity());
        let body = match (&self.note, &self.picture) {
            (Some(note), _) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(crate::host_ui::unavailable(
                    IconName::Globe,
                    "This tab isn't open",
                    note,
                    &p,
                ))
                .child(
                    Button::new("page-reopen")
                        .small()
                        .label("Open again")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.note = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
            (None, Some(picture)) => img(picture.clone()).size_full().into_any_element(),
            (None, None) => div().size_full().into_any_element(),
        };
        let page = div()
            .id("page-viewport")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(gpui_kit::white())
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy_selection(false, cx)))
            .on_action(cx.listener(|this, _: &CutSelection, _, cx| this.copy_selection(true, cx)))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(body)
            .child(
                // Where the page is, for mouse positions and its size; and
                // the text input (typing, input methods, paste) while it
                // has the focus.
                canvas(
                    move |area, _, _| bounds.set(Some(area)),
                    move |area, (), window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(area, entity), cx);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        v_flex()
            .size_full()
            .bg(p.background)
            .child(toolbar)
            .child(page)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(str::to_owned),
        }
    }

    #[test]
    fn keys_become_dom_keys() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let d = |k: Keystroke| dom_key(&k);
        assert_eq!(
            d(stroke("a", Some("a"), plain)),
            ("a".into(), "KeyA".into())
        );
        assert_eq!(
            d(stroke("a", Some("A"), shift)),
            ("A".into(), "KeyA".into())
        );
        // A chord types nothing, but still names its key.
        assert_eq!(d(stroke("a", None, shift)), ("A".into(), "KeyA".into()));
        assert_eq!(
            d(stroke("7", Some("7"), plain)),
            ("7".into(), "Digit7".into())
        );
        assert_eq!(
            d(stroke("/", Some("?"), shift)),
            ("?".into(), "Slash".into())
        );
        assert_eq!(
            d(stroke("enter", None, plain)),
            ("Enter".into(), "Enter".into())
        );
        assert_eq!(
            d(stroke("left", None, plain)),
            ("ArrowLeft".into(), "ArrowLeft".into())
        );
        assert_eq!(
            d(stroke("space", Some(" "), plain)),
            (" ".into(), "Space".into())
        );
        assert_eq!(d(stroke("f5", None, plain)), ("F5".into(), "F5".into()));
        // A key we have no code for still carries what it types.
        assert_eq!(
            d(stroke("é", Some("é"), plain)),
            ("é".into(), String::new())
        );
    }

    #[test]
    fn a_view_is_out_of_sight_only_once_its_window_draws_without_it() {
        let draw = |window: u64| FRAMES.with(|f| *f.borrow_mut().entry(window).or_default() += 1);
        draw(7);
        let seen = Seen {
            window: 7,
            frame: Seen::latest(7),
        };
        // Nothing drawn since: it is still on screen, however long that is.
        assert!(seen.in_sight());
        // Another window drawing says nothing about this one.
        draw(8);
        assert!(seen.in_sight());
        draw(7);
        assert!(!seen.in_sight());
    }

    #[test]
    fn a_pages_cursor_becomes_the_viewers_pointer() {
        use gpui_kit::CursorStyle as C;
        assert_eq!(pointer_for("text"), C::IBeam);
        assert_eq!(pointer_for("pointer"), C::PointingHand);
        assert_eq!(pointer_for("grabbing"), C::ClosedHand);
        assert_eq!(pointer_for("col-resize"), C::ResizeColumn);
        // The ordinary arrow: nothing asked for, or nothing we know.
        assert_eq!(pointer_for(""), C::Arrow);
        assert_eq!(pointer_for("zoom-in"), C::Arrow);
    }

    #[test]
    fn modifier_bits() {
        let m = Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(modifiers(&m), page_modifiers::ALT | page_modifiers::SHIFT);
        assert_eq!(modifiers(&Modifiers::default()), 0);
        assert!(is_text("a") && is_text("日本") && !is_text("\r") && !is_text(""));
    }
}
