//! The Device panel (ADR-0020): an iOS simulator or Android emulator on the
//! daemon's computer, its screen streamed as pictures like a browser tab's
//! (`page.rs`) and driven by sending touches, keys and buttons back. With
//! no device chosen it lists them, to start one or pick one that runs.
//! Closing the panel never shuts a device down: others (an agent, another
//! window) may be using it.

use super::page::{Seen, modifiers};
use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Bounds, ClipboardItem, Context, DispatchPhase, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, RenderImage, ScrollEvent, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, UTF16Selection, Window, canvas, div, img, px,
};
use loft_protocol::{
    DeviceButton, DeviceId, DeviceInfo, DeviceInput, DevicePlatform, DeviceSetting, DeviceState,
    DeviceSupport, Request, Response, ThreadId, Topic, TouchPhase,
};
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

gpui_kit::actions!(device_view, [PasteToDevice]);

pub const KEY_CONTEXT: &str = "DeviceView";
const TOOLBAR_HEIGHT: f32 = 40.;
/// Finger moves are sent at most this often.
const MOVE_EVERY: Duration = Duration::from_millis(16);
/// Wheel turns are gathered for this long and sent as one scroll.
const SCROLL_EVERY: Duration = Duration::from_millis(40);
/// A panel out of sight for this long stops watching (the device stops
/// streaming to it); drawing it again watches again.
const UNSEEN_AFTER: Duration = Duration::from_millis(1500);
/// Room around the phone in the panel.
const MARGIN: f32 = 18.;
/// Room around the phone when it floats over the chat.
const FLOAT_MARGIN: f32 = 6.;
/// The phone's frame around its screen.
const BEZEL: f32 = 7.;
/// A screen's size before its device has said (an iPhone's, in points).
const DEFAULT_SCREEN: (f32, f32) = (390., 844.);

/// What the panel has for the thread's composer.
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A picture of the screen (PNG).
    Screenshot(Vec<u8>),
    /// Float the device over the chat (out of the side panel).
    Float,
}
impl gpui_kit::EventEmitter<DeviceEvent> for DeviceView {}

struct Initialized;
impl gpui_kit::Global for Initialized {}

fn init(cx: &mut App) {
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    // In a browser its own paste brings the text
    // (`EntityInputHandler::paste`); the app reads the clipboard itself.
    if !crate::host::IS_WEB {
        let ctx = Some(KEY_CONTEXT);
        cx.bind_keys([
            KeyBinding::new("cmd-v", PasteToDevice, ctx),
            KeyBinding::new("ctrl-v", PasteToDevice, ctx),
        ]);
    }
}

pub struct DeviceView {
    store: Entity<AppStore>,
    thread: ThreadId,
    focus: FocusHandle,
    /// The devices and what can run, once the daemon has said.
    devices: Option<(Vec<DeviceInfo>, DeviceSupport)>,
    /// The device shown (none: the list).
    chosen: Option<DeviceId>,
    /// Start the chosen device once the list says it is stopped.
    boot_when_listed: bool,
    /// The chosen device as its own subscription last said.
    info: Option<DeviceInfo>,
    /// Its screen as last pictured.
    picture: Option<Arc<RenderImage>>,
    /// A picture is being decoded; the newest to arrive meanwhile waits.
    decoding: bool,
    waiting: Option<(u64, u64, Vec<u8>)>,
    /// What this panel watches now: the list, and which device.
    watching_list: bool,
    watching: Option<DeviceId>,
    /// Where the screen's area is (set while painting).
    area: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The frame of its window the panel was last painted in.
    seen: Rc<Cell<Option<Seen>>>,
    /// Lets go of the device once the panel is out of sight.
    resting: Option<Task<()>>,
    /// A finger is down on the screen, and where it was last sent.
    touching: bool,
    last_touch: (f32, f32),
    last_move: Option<Instant>,
    /// Wheel turns not sent yet: where, and how far.
    scroll: Option<(f32, f32, f32, f32)>,
    scroll_pending: bool,
    /// The appearance last asked for (the device doesn't say).
    dark: bool,
    /// What went wrong asking the daemon (a boot that failed).
    note: Option<String>,
    /// Where the device picker is (its menu opens under it).
    picker: Rc<Cell<Option<Bounds<Pixels>>>>,
    marked: Option<String>,
    /// Drawn in the card floating over the chat: the screen alone, with
    /// less room around it (the card has its own bar).
    floating: bool,
    _subs: Vec<Subscription>,
}

impl Focusable for DeviceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DeviceView {
    pub fn new(store: Entity<AppStore>, thread: ThreadId, cx: &mut Context<Self>) -> Self {
        init(cx);
        let chosen = store.read(cx).device_choice.get(&thread).cloned();
        let me = cx.entity_id();
        let subs = vec![
            // Going away stops watching (the device keeps running).
            cx.on_release(move |this, cx| {
                let id = me;
                this.store.update(cx, |s, _| s.unwatch_device(id, |_| true));
                if let Some(picture) = this.picture.take() {
                    cx.drop_image(picture, None);
                }
            }),
            // Back in touch with the daemon (or let use its devices).
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            thread,
            focus: cx.focus_handle(),
            devices: None,
            chosen,
            boot_when_listed: false,
            info: None,
            picture: None,
            decoding: false,
            waiting: None,
            watching_list: false,
            watching: None,
            area: Rc::default(),
            seen: Rc::default(),
            resting: None,
            touching: false,
            last_touch: (0., 0.),
            last_move: None,
            scroll: None,
            scroll_pending: false,
            dark: false,
            note: None,
            picker: Rc::default(),
            marked: None,
            floating: false,
            _subs: subs,
        }
    }

    /// The device shown, if one is.
    pub fn chosen(&self) -> Option<&DeviceId> {
        self.chosen.as_ref()
    }

    /// The chosen device as last known (its own state, else the list's).
    pub fn device(&self) -> Option<&DeviceInfo> {
        let id = self.chosen.as_ref()?;
        self.info
            .as_ref()
            .filter(|i| &i.id == id)
            .or_else(|| self.listed(id))
    }

    fn listed(&self, id: &DeviceId) -> Option<&DeviceInfo> {
        self.devices.as_ref()?.0.iter().find(|d| &d.id == id)
    }

    /// For the panel's tab: the device's name, and whether it is busy.
    pub fn title(&self) -> String {
        self.device()
            .map_or_else(|| "Device".to_owned(), |d| d.name.clone())
    }

    pub fn is_busy(&self) -> bool {
        self.device()
            .is_some_and(|d| matches!(d.state, DeviceState::Booting | DeviceState::ShuttingDown))
    }

    /// Show device `id` (none: the list), starting it if `boot` and it is
    /// stopped.
    pub fn choose(&mut self, id: Option<DeviceId>, boot: bool, cx: &mut Context<Self>) {
        if self.chosen != id {
            self.info = None;
            self.drop_picture(cx);
            self.touching = false;
        }
        self.note = None;
        self.chosen = id.clone();
        self.boot_when_listed = boot && id.is_some();
        let thread = self.thread.clone();
        self.store.update(cx, |s, _| match id {
            Some(id) => {
                s.device_choice.insert(thread, id);
            }
            None => {
                s.device_choice.remove(&thread);
            }
        });
        self.sync(cx);
        cx.notify();
    }

    /// Whether it is drawn floating over the chat (set by whoever draws it,
    /// before it draws).
    pub fn set_floating(&mut self, on: bool) {
        self.floating = on;
    }

    /// How tall the card floating over the chat is for `width`: the screen
    /// as wide as there is room for, and the room around it.
    pub fn floating_height(&self, width: f32) -> f32 {
        let (sw, sh) = self.screen_size();
        let room = 2. * (FLOAT_MARGIN + BEZEL);
        (width - room).max(0.) * sh / sw.max(1.) + room
    }

    fn margin(&self) -> f32 {
        if self.floating { FLOAT_MARGIN } else { MARGIN }
    }

    /// The panel is going away: stop watching (the device keeps running).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.stop_watching(cx);
    }

    // -- watching ---------------------------------------------------------------

    /// Watch what the panel shows, while it is drawn.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).pages_allowed() {
            return;
        }
        if !self.watching_list {
            self.watching_list = true;
            let view = cx.entity().downgrade();
            self.store
                .update(cx, |s, _| s.watch_device(Topic::Devices, view));
        }
        if self.resting.is_none() {
            self.rest_when_unseen(cx);
        }
        self.sync(cx);
    }

    fn stop_watching(&mut self, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        self.store.update(cx, |s, _| s.unwatch_device(id, |_| true));
        self.watching_list = false;
        self.watching = None;
        self.resting = None;
        self.touching = false;
    }

    /// Watch the chosen device, once the list says it exists (a device
    /// the daemon doesn't know would end the connection). Start it if
    /// asked to.
    fn sync(&mut self, cx: &mut Context<Self>) {
        if !self.watching_list {
            return;
        }
        let Some((list, _)) = &self.devices else {
            return;
        };
        // A device remembered from before that is gone now: the list.
        if let Some(id) = &self.chosen
            && !list.iter().any(|d| &d.id == id)
        {
            tracing::info!(device = %id, "the chosen device is gone");
            self.chosen = None;
            self.info = None;
            self.boot_when_listed = false;
            let thread = self.thread.clone();
            self.store
                .update(cx, |s, _| s.device_choice.remove(&thread));
        }
        if self.watching != self.chosen {
            let entity = cx.entity_id();
            let view = cx.entity().downgrade();
            let new = self.chosen.clone();
            self.store.update(cx, |s, _| {
                s.unwatch_device(entity, |t| matches!(t, Topic::Device(_)));
                if let Some(id) = new {
                    s.watch_device(Topic::Device(id), view);
                }
            });
            self.watching = self.chosen.clone();
        }
        if self.boot_when_listed
            && let Some((id, state)) = self.device().map(|d| (d.id.clone(), d.state))
        {
            self.boot_when_listed = false;
            if state == DeviceState::Stopped {
                self.boot(id, cx);
            }
        }
    }

    /// Check now and then whether the panel is still drawn; once it hasn't
    /// been for a while, stop watching. Drawing it again watches again.
    fn rest_when_unseen(&mut self, cx: &mut Context<Self>) {
        self.resting = Some(cx.spawn(async move |this, cx| {
            let mut gone: Option<Instant> = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let rest = this.update(cx, |this, cx| {
                    if this.seen.get().is_some_and(Seen::in_sight) {
                        gone = None;
                        return false;
                    }
                    if gone.get_or_insert_with(Instant::now).elapsed() < UNSEEN_AFTER {
                        return false;
                    }
                    tracing::debug!("the device panel is out of sight: not watching");
                    let id = cx.entity_id();
                    this.store.update(cx, |s, _| s.unwatch_device(id, |_| true));
                    this.watching_list = false;
                    this.watching = None;
                    this.touching = false;
                    true
                });
                if rest.unwrap_or(true) {
                    let _ = this.update(cx, |this, _| this.resting = None);
                    break;
                }
            }
        }));
    }

    // -- fed by the store -------------------------------------------------------
    //
    // These are called while the store hands out what the daemon sent, so
    // whatever needs the store (watching, asking, acknowledging) waits
    // until it is done.

    pub fn on_list(
        &mut self,
        devices: Vec<DeviceInfo>,
        support: DeviceSupport,
        cx: &mut Context<Self>,
    ) {
        self.devices = Some((devices, support));
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| this.sync(cx));
        })
        .detach();
        cx.notify();
    }

    pub fn on_state(&mut self, device: DeviceInfo, cx: &mut Context<Self>) {
        if self.chosen.as_ref() != Some(&device.id) {
            return;
        }
        if device.state != DeviceState::Running {
            self.drop_picture(cx);
            self.touching = false;
        }
        if let Some((list, _)) = &mut self.devices
            && let Some(listed) = list.iter_mut().find(|d| d.id == device.id)
        {
            *listed = device.clone();
        }
        self.info = Some(device);
        cx.notify();
    }

    /// A new picture. One is decoded at a time; while that happens only the
    /// newest to arrive is kept, and the daemon sends no more until this
    /// one is acknowledged.
    pub fn on_frame(&mut self, sub_id: u64, seq: u64, jpeg: Vec<u8>, cx: &mut Context<Self>) {
        if self.decoding {
            if let Some((old_sub, old_seq, _)) = self.waiting.replace((sub_id, seq, jpeg)) {
                cx.spawn(async move |this, cx| {
                    let _ = this.update(cx, |this, cx| this.ack(old_sub, old_seq, cx));
                })
                .detach();
            }
            return;
        }
        self.decoding = true;
        let decoded = crate::host::decode_jpeg(jpeg, cx);
        cx.spawn(async move |this, cx| {
            let picture = decoded.await;
            let _ = this.update(cx, |this, cx| {
                this.decoding = false;
                if let Some(picture) = picture
                    && this.watching.is_some()
                {
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

    // -- asking the daemon --------------------------------------------------------

    /// Ask, and show what went wrong if it did.
    fn ask(&mut self, body: Request, cx: &mut Context<Self>) {
        request_in(&self.store, body, cx, |this, result, cx| {
            if let Err(err) = result {
                this.note = Some(err.message);
                cx.notify();
            }
        });
    }

    /// Ask without waiting (input: a lost touch isn't worth a message).
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

    fn boot(&mut self, id: DeviceId, cx: &mut Context<Self>) {
        self.note = None;
        // It says so itself in a moment; meanwhile, show that it starts.
        if let Some((list, _)) = &mut self.devices
            && let Some(d) = list.iter_mut().find(|d| d.id == id)
        {
            d.state = DeviceState::Booting;
            d.error = None;
        }
        if let Some(info) = self.info.as_mut().filter(|i| i.id == id) {
            info.state = DeviceState::Booting;
            info.error = None;
        }
        self.ask(
            Request::DeviceBoot {
                id,
                thread_id: None,
                wait: false,
            },
            cx,
        );
        cx.notify();
    }

    fn shut_down(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.chosen.clone() {
            self.ask(Request::DeviceShutdown { id }, cx);
        }
    }

    fn input(&self, input: DeviceInput, cx: &mut Context<Self>) {
        if let Some(id) = &self.watching
            && self.is_running()
        {
            tracing::trace!(?input, "to the device");
            self.send(
                Request::DeviceInput {
                    id: id.clone(),
                    input,
                    thread_id: None,
                },
                cx,
            );
        }
    }

    fn press(&mut self, button: DeviceButton, cx: &mut Context<Self>) {
        self.input(DeviceInput::Button { button }, cx);
    }

    fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        if let Some(id) = self.chosen.clone() {
            self.dark = dark;
            self.ask(
                Request::DeviceSet {
                    id,
                    setting: DeviceSetting::Appearance { dark },
                },
                cx,
            );
            cx.notify();
        }
    }

    fn take_screenshot(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.chosen.clone() else {
            return;
        };
        request_in(
            &self.store,
            Request::DeviceCapture {
                id,
                thread_id: None,
            },
            cx,
            |this, result, cx| match result {
                Ok(Response::Picture { png }) => cx.emit(DeviceEvent::Screenshot(png.0)),
                Ok(_) => {}
                Err(err) => {
                    this.note = Some(err.message);
                    cx.notify();
                }
            },
        );
    }

    fn is_running(&self) -> bool {
        self.device()
            .is_some_and(|d| d.state == DeviceState::Running)
    }

    // -- the screen -----------------------------------------------------------------

    /// The screen's size in pixels as it is turned now.
    fn screen_size(&self) -> (f32, f32) {
        if let Some(d) = self.device()
            && d.width > 0
            && d.height > 0
        {
            return (d.width as f32, d.height as f32);
        }
        if let Some(picture) = &self.picture {
            let size = picture.size(0);
            let (w, h) = (size.width.0 as f32, size.height.0 as f32);
            if w >= 1. && h >= 1. {
                return (w, h);
            }
        }
        DEFAULT_SCREEN
    }

    /// Where the screen is drawn in the window.
    fn drawn(&self) -> Option<Bounds<Pixels>> {
        let area = self.area.get()?;
        let (x, y, w, h) = fit_screen(
            (f32::from(area.size.width), f32::from(area.size.height)),
            self.screen_size(),
            self.margin(),
        );
        (w >= 1. && h >= 1.).then(|| {
            Bounds::new(
                area.origin + gpui_kit::point(px(x), px(y)),
                gpui_kit::size(px(w), px(h)),
            )
        })
    }

    /// A window position as a fraction of the screen, if it is on it.
    fn screen_point(&self, position: Point<Pixels>, clamp: bool) -> Option<(f32, f32)> {
        let drawn = self.drawn()?;
        let (x, y) = to_fraction(
            (f32::from(position.x), f32::from(position.y)),
            (
                f32::from(drawn.origin.x),
                f32::from(drawn.origin.y),
                f32::from(drawn.size.width),
                f32::from(drawn.size.height),
            ),
        );
        if clamp {
            Some((x.clamp(0., 1.), y.clamp(0., 1.)))
        } else {
            ((0. ..=1.).contains(&x) && (0. ..=1.).contains(&y)).then_some((x, y))
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if !self.is_running() || self.picture.is_none() {
            return;
        }
        let Some((x, y)) = self.screen_point(event.position, false) else {
            return;
        };
        cx.stop_propagation();
        self.touching = true;
        self.last_touch = (x, y);
        self.last_move = Some(Instant::now());
        self.input(
            DeviceInput::Touch {
                phase: TouchPhase::Down,
                x,
                y,
            },
            cx,
        );
        cx.notify();
    }

    /// The finger moves (anywhere in the window while it is down).
    fn touch_move(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self
            .last_move
            .is_some_and(|at| now.duration_since(at) < MOVE_EVERY)
        {
            return;
        }
        let Some((x, y)) = self.screen_point(position, true) else {
            return;
        };
        if (x, y) == self.last_touch {
            return;
        }
        self.last_move = Some(now);
        self.last_touch = (x, y);
        self.input(
            DeviceInput::Touch {
                phase: TouchPhase::Move,
                x,
                y,
            },
            cx,
        );
    }

    /// The finger lifts (wherever the button was let go).
    fn touch_up(&mut self, position: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.touching) {
            return;
        }
        let (x, y) = position
            .and_then(|p| self.screen_point(p, true))
            .unwrap_or(self.last_touch);
        self.input(
            DeviceInput::Touch {
                phase: TouchPhase::Up,
                x,
                y,
            },
            cx,
        );
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((x, y)), Some(drawn)) = (self.screen_point(event.position, false), self.drawn())
        else {
            return;
        };
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(window.line_height());
        let dx = -f32::from(delta.x) / f32::from(drawn.size.width);
        let dy = -f32::from(delta.y) / f32::from(drawn.size.height);
        let (_, _, sx, sy) = self.scroll.unwrap_or_default();
        self.scroll = Some((x, y, sx + dx, sy + dy));
        if !std::mem::replace(&mut self.scroll_pending, true) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SCROLL_EVERY).await;
                let _ = this.update(cx, |this, cx| {
                    this.scroll_pending = false;
                    if let Some((x, y, dx, dy)) = this.scroll.take()
                        && (dx != 0. || dy != 0.)
                    {
                        this.input(DeviceInput::Scroll { x, y, dx, dy }, cx);
                    }
                });
            })
            .detach();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_some() || !self.is_running() {
            return;
        }
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        // The app's own shortcuts (and the browser's paste) go on.
        if m.platform || m.control {
            return;
        }
        // Text goes through the platform's text input (input methods,
        // dead keys), and arrives in `replace_text_in_range`.
        if let Some(key) = device_key(&keystroke.key) {
            self.input(
                DeviceInput::Key {
                    key: key.to_owned(),
                    modifiers: modifiers(m),
                },
                cx,
            );
            cx.stop_propagation();
        }
    }

    fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.input(
                DeviceInput::Text {
                    text: text.to_owned(),
                },
                cx,
            );
        }
    }

    fn paste_clipboard(&mut self, _: &PasteToDevice, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.type_text(&text, cx);
        }
    }
}

/// Where a screen of `screen` pixels is drawn in an area of `area` view
/// pixels: its offset and size, scaled to fit (up or down) inside a margin
/// and a bezel, keeping its shape, centred.
fn fit_screen(area: (f32, f32), screen: (f32, f32), margin: f32) -> (f32, f32, f32, f32) {
    let (w, h) = area;
    let (sw, sh) = screen;
    if sw < 1. || sh < 1. {
        return (0., 0., 0., 0.);
    }
    let room = 2. * (margin + BEZEL);
    let s = ((w - room) / sw).min((h - room) / sh).max(0.);
    let (dw, dh) = (sw * s, sh * s);
    ((w - dw) / 2., (h - dh) / 2., dw, dh)
}

/// A point as a fraction of a rectangle `(x, y, width, height)`.
fn to_fraction(point: (f32, f32), rect: (f32, f32, f32, f32)) -> (f32, f32) {
    let (x, y, w, h) = rect;
    ((point.0 - x) / w.max(1.), (point.1 - y) / h.max(1.))
}

/// The DOM name of a key that types no text, for `DeviceInput::Key`.
fn device_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "enter" => "Enter",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "tab" => "Tab",
        "escape" => "Escape",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        _ => return None,
    })
}

/// How round a screen's corners are, by how wide it is drawn.
fn corner_radius(platform: Option<DevicePlatform>, drawn: (f32, f32)) -> f32 {
    let short = drawn.0.min(drawn.1);
    match platform {
        Some(DevicePlatform::Android) => short * 0.07,
        _ => short * 0.12,
    }
}

fn state_label(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Stopped => "Stopped",
        DeviceState::Booting => "Starting…",
        DeviceState::Running => "Running",
        DeviceState::ShuttingDown => "Shutting down…",
    }
}

impl EntityInputHandler for DeviceView {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.is_running()
    }

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
        if let Some(text) = item.text() {
            self.type_text(&text, cx);
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
        // Enter and Tab come as keys (`on_key_down`).
        if !text.chars().any(char::is_control) {
            self.type_text(text, cx);
        }
    }

    /// What an input method is composing: it reaches the device once
    /// committed.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let drawn = self.drawn()?;
        let (x, y) = self.last_touch;
        Some(Bounds::new(
            drawn.origin + gpui_kit::point(drawn.size.width * x, drawn.size.height * y),
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

// -- drawing ------------------------------------------------------------------------

impl DeviceView {
    fn toolbar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let device = self.device().cloned();
        let running = self.is_running();
        let android = device
            .as_ref()
            .is_some_and(|d| d.platform == DevicePlatform::Android);
        let anchor = self.picker.clone();
        let label = device
            .as_ref()
            .map_or_else(|| "Devices".to_owned(), |d| d.name.clone());
        let picker = div()
            .relative()
            .flex_shrink(1.)
            .min_w_0()
            .child(
                h_flex()
                    .id("device-picker")
                    .h(px(26.))
                    .px_2()
                    .gap_1p5()
                    .items_center()
                    .rounded(px(6.))
                    .text_sm()
                    .cursor_pointer()
                    .text_color(p.foreground)
                    .hover(|d| d.bg(p.hover))
                    .child(
                        Icon::new(IconName::Smartphone)
                            .small()
                            .text_color(p.muted_foreground),
                    )
                    .child(div().min_w_0().truncate().child(label))
                    .when_some(device.as_ref(), |d, device| {
                        d.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(p.muted_foreground)
                                .child(device.os.clone()),
                        )
                    })
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(p.muted_foreground),
                    )
                    .claims_press()
                    .on_click(cx.listener(|this, _, window, cx| this.picker_menu(window, cx))),
            )
            .child(
                canvas(
                    move |bounds, _, _| anchor.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        let tool = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id)
                .cursor_pointer()
                .ghost()
                .small()
                .icon(icon)
                .tooltip(tip)
        };
        let button = |id: &'static str, icon: IconName, tip: &'static str, b: DeviceButton| {
            tool(id, icon, tip)
                .disabled(!running)
                .on_click(cx.listener(move |this, _, _, cx| this.press(b, cx)))
        };
        let dark = self.dark;
        h_flex()
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .px_2()
            .gap_0p5()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .child(picker)
            .child(div().flex_1())
            .when(device.is_some(), |bar| {
                bar.when(android, |bar| {
                    bar.child(button(
                        "device-back",
                        IconName::ArrowLeft,
                        "Back",
                        DeviceButton::Back,
                    ))
                })
                .child(button(
                    "device-home",
                    IconName::House,
                    "Home",
                    DeviceButton::Home,
                ))
                .when(android, |bar| {
                    bar.child(button(
                        "device-recents",
                        IconName::Square,
                        "Recent apps",
                        DeviceButton::Recents,
                    ))
                })
                .child(button(
                    "device-rotate-left",
                    IconName::RotateCcw,
                    "Rotate left",
                    DeviceButton::RotateLeft,
                ))
                .child(button(
                    "device-rotate-right",
                    IconName::RotateCw,
                    "Rotate right",
                    DeviceButton::RotateRight,
                ))
                .child(
                    tool(
                        "device-shot",
                        IconName::Camera,
                        "Screenshot to the composer",
                    )
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, _, cx| this.take_screenshot(cx))),
                )
                .child(
                    tool(
                        "device-appearance",
                        if dark { IconName::Sun } else { IconName::Moon },
                        if dark {
                            "Light appearance"
                        } else {
                            "Dark appearance"
                        },
                    )
                    .disabled(!running)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_dark(!dark, cx))),
                )
                .child(
                    tool(
                        "device-float",
                        IconName::PictureInPicture2,
                        "Float over the chat",
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DeviceEvent::Float))),
                )
                .child(
                    tool("device-power", IconName::Power, "Shut down")
                        .disabled(!running)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_shutdown(window, cx)),
                        ),
                )
            })
            .into_any_element()
    }

    /// The picker's menu: every device, and the list.
    fn picker_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use loft_browser::menu::{self, Entry};
        let Some(anchor) = self.picker.get() else {
            return;
        };
        let view = cx.entity().downgrade();
        let mut entries = Vec::new();
        for d in self
            .devices
            .as_ref()
            .map(|(l, _)| l.as_slice())
            .unwrap_or(&[])
        {
            let (view, id) = (view.clone(), d.id.clone());
            let label = match d.state {
                DeviceState::Running => format!("{} · {}", d.name, d.os),
                state => format!("{} · {} · {}", d.name, d.os, state_label(state)),
            };
            entries.push(
                Entry::item(label, IconName::Smartphone, move |cx| {
                    let id = id.clone();
                    let _ = view.update(cx, |v, cx| v.choose(Some(id), false, cx));
                })
                .checked(self.chosen.as_ref() == Some(&d.id)),
            );
        }
        if !entries.is_empty() {
            entries.push(Entry::Separator);
        }
        entries.push(Entry::item(
            "All devices",
            IconName::ListFilter,
            move |cx| {
                let _ = view.update(cx, |v, cx| v.choose(None, false, cx));
            },
        ));
        let at = gpui_kit::point(anchor.origin.x, anchor.origin.y + anchor.size.height);
        menu::open_at(window, at, entries, cx);
    }

    fn confirm_shutdown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
        let name = self.title();
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .centered()
                .title(format!("Shut down {name}?"))
                .child("Apps running on it stop. Anything using it, an agent included, loses it.")
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel").cursor_pointer().label("Cancel")),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("ok")
                                    .cursor_pointer()
                                    .danger()
                                    .label("Shut Down"),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = view.update(cx, |v, cx| v.shut_down(cx));
                    true
                })
        });
    }

    /// No device chosen: the devices, by platform.
    fn render_list(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some((devices, support)) = &self.devices else {
            return h_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(Spinner::new().xsmall().color(p.muted_foreground))
                .child("Looking for simulators and emulators…")
                .into_any_element();
        };
        if devices.is_empty() && !support.ios && !support.android {
            let mut detail = Vec::new();
            detail.extend(support.ios_problem.clone());
            detail.extend(support.android_problem.clone());
            if detail.is_empty() {
                detail.push(
                    "Install Xcode for iOS simulators, or the Android SDK for emulators.".into(),
                );
            }
            return crate::host_ui::unavailable(
                IconName::Smartphone,
                "No simulators or emulators here",
                &detail.join(" "),
                p,
            );
        }
        let mut body = v_flex()
            .id("device-list")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_4();
        for (platform, title, can, problem, none) in [
            (
                DevicePlatform::Ios,
                "iOS Simulators",
                support.ios,
                support.ios_problem.as_deref(),
                "No simulators. Add one in Xcode: Window ▸ Devices and Simulators.",
            ),
            (
                DevicePlatform::Android,
                "Android Emulators",
                support.android,
                support.android_problem.as_deref(),
                "No emulators. Add one in Android Studio's Device Manager.",
            ),
        ] {
            let mine: Vec<&DeviceInfo> =
                devices.iter().filter(|d| d.platform == platform).collect();
            let mut group = v_flex().gap_1().child(
                div()
                    .px_2()
                    .text_xs()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(p.muted_foreground)
                    .child(title),
            );
            if !can && mine.is_empty() {
                group = group.child(
                    div().px_2().text_xs().text_color(p.faint_foreground).child(
                        problem
                            .unwrap_or("Not available on this computer.")
                            .to_owned(),
                    ),
                );
            } else if mine.is_empty() {
                group = group.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(none),
                );
            }
            for d in mine {
                group = group.child(self.device_row(d, p, cx));
            }
            body = body.child(group);
        }
        body.into_any_element()
    }

    fn device_row(&self, d: &DeviceInfo, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (open_id, boot_id) = (d.id.clone(), d.id.clone());
        let state: AnyElement = match d.state {
            DeviceState::Stopped => {
                Button::new(SharedString::from(format!("device-start-{}", d.id)))
                    .cursor_pointer()
                    .small()
                    .icon(IconName::Play)
                    .label("Start")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose(Some(boot_id.clone()), true, cx);
                    }))
                    .into_any_element()
            }
            DeviceState::Booting | DeviceState::ShuttingDown => h_flex()
                .gap_1p5()
                .items_center()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(Spinner::new().xsmall().color(p.muted_foreground))
                .child(state_label(d.state))
                .into_any_element(),
            DeviceState::Running => h_flex()
                .gap_1p5()
                .items_center()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(div().size(px(7.)).rounded_full().bg(p.success))
                .child("Running")
                .into_any_element(),
        };
        h_flex()
            .id(SharedString::from(format!("device-row-{}", d.id)))
            .min_h(px(44.))
            .px_2()
            .gap_2p5()
            .items_center()
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(p.hover))
            .child(
                Icon::new(IconName::Smartphone)
                    .small()
                    .text_color(p.muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.foreground)
                            .truncate()
                            .child(d.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .truncate()
                            .child(d.os.clone()),
                    )
                    .when_some(d.error.clone(), |c, e| {
                        c.child(div().text_xs().text_color(p.danger).child(e))
                    }),
            )
            .child(div().flex_none().child(state))
            .claims_press()
            .on_click(
                cx.listener(move |this, _, _, cx| this.choose(Some(open_id.clone()), false, cx)),
            )
            .into_any_element()
    }

    /// A device chosen: its screen, in a phone's frame, fitted to the panel.
    fn render_screen(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let device = self.device().cloned();
        let state = device.as_ref().map(|d| d.state);
        let error = self
            .note
            .clone()
            .or_else(|| device.as_ref().and_then(|d| d.error.clone()));
        let drawn = self.drawn();
        let area = self.area.get();
        let (entity, area_cell, seen, focus) = (
            cx.entity(),
            self.area.clone(),
            self.seen.clone(),
            self.focus.clone(),
        );
        let id = entity.entity_id();
        let touching = self.touching;

        // What shows over a screen with nothing on it yet.
        let muted = |text: &'static str| {
            v_flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(gpui_kit::white().opacity(0.75))
                .child(
                    Spinner::new()
                        .small()
                        .color(gpui_kit::white().opacity(0.75)),
                )
                .child(text)
                .into_any_element()
        };
        let over: Option<AnyElement> = if let Some(error) = &error {
            Some(
                v_flex()
                    .max_w(px(280.))
                    .items_center()
                    .gap_3()
                    .p_3()
                    .rounded(px(10.))
                    .bg(p.background)
                    .border_1()
                    .border_color(p.border)
                    .child(Icon::new(IconName::TriangleAlert).text_color(p.danger))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.foreground)
                            .child(error.clone()),
                    )
                    .child(
                        Button::new("device-retry")
                            .cursor_pointer()
                            .small()
                            .label("Try again")
                            .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    )
                    .into_any_element(),
            )
        } else {
            match state {
                None => Some(muted("Connecting…")),
                Some(DeviceState::Booting) => Some(muted("Starting…")),
                Some(DeviceState::ShuttingDown) => Some(muted("Shutting down…")),
                Some(DeviceState::Stopped) => {
                    let name = device.as_ref().map_or_else(String::new, |d| d.name.clone());
                    Some(
                        v_flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .text_color(gpui_kit::white().opacity(0.75))
                            .child(format!("{name} is off"))
                            .child(
                                Button::new("device-start")
                                    .cursor_pointer()
                                    .small()
                                    .icon(IconName::Play)
                                    .label("Start")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(id) = this.chosen.clone() {
                                            this.boot(id, cx);
                                        }
                                    })),
                            )
                            .into_any_element(),
                    )
                }
                Some(DeviceState::Running) if self.picture.is_none() => {
                    Some(muted("Waiting for the screen…"))
                }
                Some(DeviceState::Running) => None,
            }
        };

        let phone = match (drawn, area) {
            (Some(drawn), Some(area)) => {
                let (w, h) = (f32::from(drawn.size.width), f32::from(drawn.size.height));
                let r = corner_radius(device.as_ref().map(|d| d.platform), (w, h));
                let screen = match (&self.picture, state) {
                    (Some(picture), Some(DeviceState::Running)) => img(picture.clone())
                        .size_full()
                        .rounded(px(r))
                        .into_any_element(),
                    _ => div()
                        .size_full()
                        .rounded(px(r))
                        .bg(gpui_kit::rgb(0x111114))
                        .into_any_element(),
                };
                Some(
                    div()
                        .absolute()
                        .left(drawn.origin.x - area.origin.x - px(BEZEL))
                        .top(drawn.origin.y - area.origin.y - px(BEZEL))
                        .w(drawn.size.width + px(2. * BEZEL))
                        .h(drawn.size.height + px(2. * BEZEL))
                        .p(px(BEZEL))
                        .rounded(px(r + BEZEL))
                        .bg(gpui_kit::rgb(0x0b0b0d))
                        .border_1()
                        .border_color(p.border)
                        .shadow_lg()
                        .child(screen)
                        .into_any_element(),
                )
            }
            _ => None,
        };

        div()
            .id("device-screen")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .bg(p.muted)
            .when(self.is_running() && self.picture.is_some(), |d| {
                d.cursor_pointer()
            })
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::paste_clipboard))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_scroll(cx.listener(Self::on_scroll))
            .children(phone)
            .when_some(over, |d, over| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(over),
                )
            })
            .child(
                // Where the screen's area is, the window's mouse while a
                // finger is down, and the text input while it has focus.
                canvas(
                    move |bounds, window, cx| {
                        seen.set(Some(Seen::now(window)));
                        let before = area_cell.replace(Some(bounds));
                        if before != Some(bounds) {
                            cx.notify(id);
                        }
                    },
                    move |bounds, (), window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                        if touching {
                            let e = entity.clone();
                            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                e.update(cx, |v, cx| match ev.pressed_button {
                                    Some(MouseButton::Left) => v.touch_move(ev.position, cx),
                                    // Let go where we didn't hear of it.
                                    _ => v.touch_up(None, cx),
                                });
                            });
                            let e = entity.clone();
                            window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                                if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left
                                {
                                    e.update(cx, |v, cx| v.touch_up(Some(ev.position), cx));
                                }
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }

    /// "Try again" on an error: start it again if it is off, else watch
    /// it again.
    fn retry(&mut self, cx: &mut Context<Self>) {
        self.note = None;
        if let Some(info) = self.info.as_mut() {
            info.error = None;
        }
        match self.device().map(|d| (d.id.clone(), d.state)) {
            Some((id, DeviceState::Stopped)) => self.boot(id, cx),
            _ => {
                let entity = cx.entity_id();
                self.store.update(cx, |s, _| {
                    s.unwatch_device(entity, |t| matches!(t, Topic::Device(_)))
                });
                self.watching = None;
                self.sync(cx);
            }
        }
        cx.notify();
    }
}

impl Render for DeviceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        if !self.store.read(cx).pages_allowed() {
            return crate::host_ui::unavailable(
                IconName::Smartphone,
                "Devices are off for web access",
                "Turn the browser on in the desktop app: Settings ▸ Web access.",
                &p,
            );
        }
        if self.store.read(cx).client().is_some() {
            self.watch(cx);
        }
        // Its area isn't known before it is drawn once: draw again then.
        if self.chosen.is_some() && self.area.get().is_none() {
            window.request_animation_frame();
        }
        let body = if self.chosen.is_some() {
            self.render_screen(&p, cx)
        } else {
            // The list too keeps the panel watched while drawn.
            let seen = self.seen.clone();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(self.render_list(&p, cx))
                .child(
                    canvas(
                        move |_, window, _| seen.set(Some(Seen::now(window))),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .bg(p.background)
            .when(!self.floating, |d| d.child(self.toolbar(&p, cx)))
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_is_fitted_into_the_panel_keeping_its_shape() {
        let room = 2. * (MARGIN + BEZEL);
        // A tall phone in a wide panel: as tall as there is room for.
        let (x, y, w, h) = fit_screen((800., 600.), (390., 844.), MARGIN);
        assert!((h - (600. - room)).abs() < 0.01);
        assert!((w / h - 390. / 844.).abs() < 0.001);
        assert!((x - (800. - w) / 2.).abs() < 0.01);
        assert!((y - (600. - h) / 2.).abs() < 0.01);
        // Turned: as wide as there is room for.
        let (_, _, w, h) = fit_screen((400., 900.), (844., 390.), MARGIN);
        assert!((w - (400. - room)).abs() < 0.01);
        assert!((w / h - 844. / 390.).abs() < 0.001);
        // Nothing known yet, or no room: nothing drawn.
        assert_eq!(fit_screen((400., 900.), (0., 0.), MARGIN), (0., 0., 0., 0.));
        let (_, _, w, h) = fit_screen((10., 10.), (390., 844.), MARGIN);
        assert_eq!((w, h), (0., 0.));
    }

    #[test]
    fn touches_are_fractions_of_the_screen() {
        let rect = (100., 50., 200., 400.);
        assert_eq!(to_fraction((100., 50.), rect), (0., 0.));
        assert_eq!(to_fraction((300., 450.), rect), (1., 1.));
        assert_eq!(to_fraction((150., 150.), rect), (0.25, 0.25));
        // Outside it: beyond 0..1, for the caller to clamp or ignore.
        let (x, _) = to_fraction((50., 60.), rect);
        assert!(x < 0.);
    }

    #[test]
    fn keys_without_text_have_dom_names() {
        assert_eq!(device_key("enter"), Some("Enter"));
        assert_eq!(device_key("backspace"), Some("Backspace"));
        assert_eq!(device_key("left"), Some("ArrowLeft"));
        assert_eq!(device_key("tab"), Some("Tab"));
        assert_eq!(device_key("escape"), Some("Escape"));
        assert_eq!(device_key("a"), None);
        assert_eq!(device_key("space"), None);
    }

    #[test]
    fn phones_are_rounder_than_android_ones() {
        let ios = corner_radius(Some(DevicePlatform::Ios), (300., 600.));
        let android = corner_radius(Some(DevicePlatform::Android), (300., 600.));
        assert!(ios > android);
        // By the short side, whichever way it is turned.
        assert_eq!(corner_radius(Some(DevicePlatform::Ios), (600., 300.)), ios);
    }
}
