//! A view an agent drew, shown as the real page (ADR-0018): a native web
//! view the app hosts itself ([`loft_browser::ViewHost`]), where the
//! platform has one. Clicks, hover, the pointer's shape, selection and
//! typing are the system's and the web view's own, with nothing of ours in
//! between.
//!
//! A native view draws above everything the app draws, so the page is only
//! *live* under the pointer. The rest of the time the row shows a picture
//! of it, which menus can cover and which scrolls in step with the thread;
//! the page itself waits out of sight, still running.
//!
//! What this file decides is only when the page is live. It never makes up
//! input for it: the one thing it passes on is a press (with its drag and
//! release) that landed on the picture a moment before the page came in.

use crate::panel::page::{PageResized, Seen, act};
use crate::spinner::Spinner;
use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::component::{Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Bounds, Context, DispatchPhase, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Render, RenderImage, ScrollEvent, Styled as _, Task, Window, canvas, div, img, px,
};
use loft_browser::{Corners, HostEvent, HostPicture, ViewHost};
use loft_protocol::{ItemId, PageAction, Request, Response, ThreadId, ViewTheme};
use serde_json::Value;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

/// How tall a view is drawn before its page has said how tall it is.
const FIRST_HEIGHT: u32 = 120;
/// The card's corner radius, less its border.
const RADIUS: f32 = 11.;
/// A picture is taken this long after the page last changed size or was
/// written to, so one burst of changes makes one picture.
const SETTLE: Duration = Duration::from_millis(120);
/// While a page that isn't live keeps changing by itself (it animates), its
/// picture is taken again this often.
const MOVING_EVERY: Duration = Duration::from_millis(70);
/// The band at the bottom of the thread where the list draws its fade and
/// its jump button, in rems: the page is not shown there (its picture is),
/// so both stay on top of it.
const BAND: f32 = 3.;
/// After the thread scrolls, the pointer has to move again (or press) for
/// the page under it to come in: a scroll is a run of events, and a page
/// that came and went with each would flicker.
const QUIET_AFTER_SCROLL: Duration = Duration::from_millis(300);
/// A view that hasn't been on screen for this long gives up its page (a web
/// view is a process of its own). Its last picture stays, and the page comes
/// back, with what it remembered, when the view is drawn again.
const UNSEEN: Duration = Duration::from_secs(30);

/// Whether views are shown natively here, or as the daemon's streamed page.
/// (`AGENTLOFT_VIEWS=stream` asks for the streamed page anyway.)
pub(crate) fn native() -> bool {
    cfg!(target_os = "macos")
        && !crate::host::IS_WEB
        && std::env::var_os("AGENTLOFT_VIEWS").is_none_or(|v| v != "stream")
}

pub struct EmbedView {
    store: Entity<AppStore>,
    thread: ThreadId,
    item: ItemId,
    /// The page, while the view has been on screen lately.
    host: Option<ViewHost>,
    /// Counts the pages this view has had: what an earlier one was asked
    /// is nothing to the one there is now.
    epoch: u64,
    /// The page's empty document is in and nothing can load that shouldn't.
    ready: bool,
    /// What the view asked to have remembered; `None` until the daemon has
    /// said (the page isn't written before).
    saved: Option<Value>,
    /// The agent's HTML as its item has it now, and whether that is all.
    html: String,
    done: bool,
    /// What the page has been given of it.
    opened: bool,
    written: String,
    closed: bool,
    writing: bool,
    write_again: bool,
    height: u32,
    /// The real page is in sight (the pointer is on it).
    live: bool,
    /// The pointer left: the page goes once its picture as it is now is in.
    leaving: bool,
    /// The thread scrolled a moment ago (see [`QUIET_AFTER_SCROLL`]).
    scrolled: Rc<Cell<Option<Instant>>>,
    /// The keyboard is in a field of the page: it stays, wherever the
    /// pointer goes, or what is typed would go nowhere.
    typing: bool,
    /// A press that landed on the picture was handed to the page: its drag
    /// and its release are the page's too (the system sends those to
    /// whoever got the press, which was the app).
    handed: Rc<Cell<bool>>,
    /// The app's own focus while the page has the keyboard. Without it the
    /// composer would still be what the app thinks is focused, and would
    /// take Cmd+A, Cmd+C and Cmd+V meant for the page.
    focus: FocusHandle,
    _blur: gpui_kit::Subscription,
    /// The page as last pictured: what the row shows while it isn't live.
    still: Option<Arc<RenderImage>>,
    /// That picture's pixels, hashed: a page whose pictures differ is moving.
    print: u64,
    picturing: bool,
    picture_again: bool,
    /// The page was just left: whatever its next picture shows, it is
    /// looked at once more (it may still be on its way back from how it
    /// looked under the pointer).
    look_again: bool,
    settle: Option<Task<()>>,
    /// The look the page was last given.
    themed: Option<ViewTheme>,
    /// Where the row is drawn, and what cuts it off (the list's edges,
    /// less the band at its bottom).
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    clip: Rc<Cell<Option<Bounds<Pixels>>>>,
    seen: Rc<Cell<Option<Seen>>>,
    /// The view reaches into the band at the bottom of the list, where its
    /// picture shows even while the page is live: so the picture is kept
    /// up with the page there.
    banded: Rc<Cell<bool>>,
    /// Whether the page is the top of its card (no title over it).
    flush_top: Rc<Cell<bool>>,
    events: Option<Task<()>>,
    _resting: Task<()>,
}

impl gpui_kit::EventEmitter<PageResized> for EmbedView {}

impl EmbedView {
    /// The view in `item`, on a page that `events` come from.
    pub fn new(
        store: Entity<AppStore>,
        thread: ThreadId,
        item: ItemId,
        host: ViewHost,
        events: UnboundedReceiver<HostEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = Some(Self::listen(events, 0, window, cx));
        let focus = cx.focus_handle();
        // The app's focus went elsewhere (a click in the composer, a
        // shortcut): the keyboard goes with it.
        let _blur = cx.on_blur(&focus, window, |this, _, _| {
            if let Some(host) = &this.host {
                host.release_focus();
            }
        });
        // Out of sight for a while: the page goes, its picture stays.
        let _resting = cx.spawn(async move |this, cx| {
            let mut gone: Option<Instant> = None;
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.host.is_none()
                        || this.live
                        || this.seen.get().is_some_and(Seen::in_sight)
                    {
                        gone = None;
                    } else if gone.get_or_insert_with(Instant::now).elapsed() >= UNSEEN {
                        gone = None;
                        this.unload(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        request_in(
            &store,
            Request::ViewState {
                thread_id: thread.clone(),
                item_id: item.clone(),
            },
            cx,
            |this, result, cx| {
                this.saved = Some(match result {
                    Ok(Response::Json { value }) => value,
                    _ => Value::Null,
                });
                this.write(cx);
            },
        );
        Self {
            store,
            thread,
            item,
            host: Some(host),
            epoch: 0,
            ready: false,
            saved: None,
            html: String::new(),
            done: false,
            opened: false,
            written: String::new(),
            closed: false,
            writing: false,
            write_again: false,
            height: FIRST_HEIGHT,
            live: false,
            leaving: false,
            scrolled: Rc::default(),
            typing: false,
            handed: Rc::default(),
            focus,
            _blur,
            still: None,
            print: 0,
            picturing: false,
            picture_again: false,
            look_again: false,
            settle: None,
            themed: None,
            bounds: Rc::default(),
            clip: Rc::default(),
            seen: Rc::default(),
            banded: Rc::default(),
            flush_top: Rc::default(),
            events,
            _resting,
        }
    }

    /// Hear what page number `epoch` says.
    fn listen(
        mut events: UnboundedReceiver<HostEvent>,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                let heard = this.update_in(cx, |this, window, cx| {
                    if this.epoch == epoch {
                        this.on_host(event, window, cx);
                    }
                });
                if heard.is_err() {
                    break;
                }
            }
        })
    }

    /// Give up the page (it has been out of sight for a while). What it
    /// looked like stays, and so does everything needed to make it again.
    fn unload(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("a view out of sight gives up its page");
        self.host = None;
        self.events = None;
        self.epoch += 1;
        self.ready = false;
        self.opened = false;
        self.written.clear();
        self.closed = false;
        self.writing = false;
        self.write_again = false;
        self.picturing = false;
        self.picture_again = false;
        self.look_again = false;
        self.live = false;
        self.leaving = false;
        self.typing = false;
        self.handed.set(false);
        cx.notify();
    }

    /// The view is drawn again and has no page: make one.
    fn wake(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (events, heard) = futures::channel::mpsc::unbounded();
        let origin = loft_view::origin(self.item.as_str());
        match ViewHost::new(window, &origin, loft_view::ALLOWED_HOSTS, events) {
            Ok(host) => {
                self.host = Some(host);
                self.events = Some(Self::listen(heard, self.epoch, window, cx));
            }
            Err(err) => tracing::debug!(err = %format!("{err:#}"), "no page for a view"),
        }
    }

    pub fn flush_top(&self) -> Rc<Cell<bool>> {
        self.flush_top.clone()
    }

    /// Where the page is drawn in its window, once it has been.
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds.get()
    }

    pub fn host(&self) -> Option<&ViewHost> {
        self.host.as_ref()
    }

    /// Whether the page was already told this HTML.
    pub fn has(&self, html: &str, done: bool) -> bool {
        self.done == done && self.html.len() == html.len() && self.html == html
    }

    /// The agent's HTML as it is now (it grows while the call is written).
    pub fn set_html(&mut self, html: &str, done: bool, cx: &mut Context<Self>) {
        if self.has(html, done) {
            return;
        }
        self.html.clear();
        self.html.push_str(html);
        self.done = done;
        self.write(cx);
    }

    /// Give the page what it hasn't got yet of the HTML: all of it into a
    /// fresh document, or only what was added since.
    fn write(&mut self, cx: &mut Context<Self>) {
        if !self.ready {
            return;
        }
        let Some(saved) = &self.saved else {
            return;
        };
        if self.writing {
            self.write_again = true;
            return;
        }
        let write = |text: &str| format!("document.write({})", Value::String(text.to_owned()));
        let follows = self.opened && self.html.starts_with(&self.written);
        let afresh = !follows || (self.closed && self.html.len() > self.written.len());
        // (script, in the bridge's world?)
        let mut steps: Vec<(String, bool)> = Vec::new();
        if afresh {
            let theme = crate::view::theme(cx);
            // Opening the document drops every listener on it, so the
            // bridge goes in after.
            steps.push(("document.open()".into(), false));
            steps.push((loft_view::BRIDGE_JS.into(), true));
            steps.push((write(&loft_view::head(&theme, saved)), false));
            self.themed = Some(theme);
            if !self.html.is_empty() {
                steps.push((write(&self.html), false));
            }
            if self.done {
                steps.push(("document.close()".into(), false));
            }
        } else {
            let more = &self.html[self.written.len()..];
            if !more.is_empty() {
                steps.push((write(more), false));
            }
            if self.done && !self.closed {
                steps.push(("document.close()".into(), false));
            }
        }
        if steps.is_empty() {
            return;
        }
        let (written, closed) = (self.html.clone(), self.done);
        let Some(host) = self.host.clone() else {
            return;
        };
        let epoch = self.epoch;
        self.writing = true;
        cx.spawn(async move |this, cx| {
            let mut failed = None;
            for (script, bridge) in steps {
                let ran = if bridge {
                    host.run_bridge(&script)
                } else {
                    host.run(&script)
                };
                match ran.await {
                    Ok(Ok(())) => {}
                    Ok(Err(why)) => {
                        failed = Some(why);
                        break;
                    }
                    // The page is gone, and this view with it.
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.epoch != epoch {
                    return;
                }
                this.writing = false;
                match failed {
                    Some(why) => {
                        tracing::warn!(%why, "a view's page wouldn't take its HTML");
                        this.opened = false;
                    }
                    None => {
                        this.opened = true;
                        this.written = written;
                        this.closed = closed;
                    }
                }
                if std::mem::take(&mut this.write_again) {
                    this.write(cx);
                }
                this.picture_soon(cx);
            });
        })
        .detach();
    }

    fn on_host(&mut self, event: HostEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            HostEvent::Ready => {
                // An empty document (the first, or the page loaded anew):
                // everything is to be written.
                self.ready = true;
                self.opened = false;
                self.written.clear();
                self.closed = false;
                self.write(cx);
            }
            HostEvent::Message(message) => {
                let Ok(call) = serde_json::from_str::<Value>(&message) else {
                    return;
                };
                let text = || call["text"].as_str().unwrap_or_default().to_owned();
                match call["kind"].as_str() {
                    Some("size") => {
                        let Some(wanted) = call["height"].as_u64() else {
                            return;
                        };
                        let wanted = wanted.min(u64::from(u32::MAX)) as u32;
                        // (More than the tallest a view gets scrolls inside.)
                        let height = wanted.clamp(loft_view::MIN_HEIGHT, loft_view::MAX_HEIGHT);
                        if height != self.height {
                            self.height = height;
                            cx.emit(PageResized);
                            cx.notify();
                        }
                        self.picture_soon(cx);
                    }
                    Some("state") => {
                        if self.live && self.banded.get() {
                            self.picture_soon(cx);
                        }
                        let state = call["value"].clone();
                        if state.to_string().len() > loft_view::MAX_STATE {
                            return;
                        }
                        self.saved = Some(state.clone());
                        request_in(
                            &self.store,
                            Request::ViewKeep {
                                thread_id: self.thread.clone(),
                                item_id: self.item.clone(),
                                state,
                            },
                            cx,
                            |_, _, _| {},
                        );
                    }
                    Some("typing") => self.typing = call["value"].as_bool().unwrap_or(false),
                    // The page took the keyboard (it was clicked into): the
                    // app's focus follows, out of wherever it was.
                    Some("focused") => {
                        if call["value"].as_bool().unwrap_or(false)
                            && !self.focus.is_focused(window)
                        {
                            window.focus(&self.focus, cx);
                        }
                    }
                    Some("reach") => {
                        let way = |i: usize| call["value"][i].as_bool().unwrap_or(false);
                        if let Some(host) = &self.host {
                            host.set_reach([way(0), way(1), way(2), way(3)]);
                        }
                    }
                    // Something in the page scrolled: the part of it that
                    // is a picture follows.
                    Some("scrolled") => {
                        if self.live && self.banded.get() {
                            self.picture(cx);
                        }
                    }
                    // The page changed by its own doing: so does whatever
                    // of it is a picture.
                    Some("changed") => {
                        if !self.live || self.banded.get() {
                            self.picture_soon(cx);
                        }
                    }
                    Some("send") => self.act(PageAction::Send { text: text() }, cx),
                    Some("copy") => self.act(PageAction::Copy { text: text() }, cx),
                    Some("open") => self.act(PageAction::Open { target: text() }, cx),
                    _ => {}
                }
            }
            HostEvent::Open(target) => self.act(PageAction::Open { target }, cx),
            // The thread scrolls: the page can't keep up with that as
            // smoothly as its picture does.
            HostEvent::Wheel => self.rest(cx),
        }
    }

    fn act(&mut self, action: PageAction, cx: &mut Context<Self>) {
        act(&self.store, &self.thread, action, cx);
    }

    // -- the page's picture ------------------------------------------------------

    fn picture_soon(&mut self, cx: &mut Context<Self>) {
        self.settle = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let _ = this.update(cx, |this, cx| this.picture(cx));
        }));
    }

    /// Take the page's picture as it is now. One at a time.
    fn picture(&mut self, cx: &mut Context<Self>) {
        if !self.opened {
            return;
        }
        if self.picturing {
            self.picture_again = true;
            return;
        }
        let Some(host) = &self.host else {
            return;
        };
        self.picturing = true;
        let (taken, epoch) = (host.snapshot(), self.epoch);
        // (Asked for before this picture was taken: it is this one that
        // is looked at again, and this one the page leaves on, not one
        // that was already on its way.)
        let look_again = std::mem::take(&mut self.look_again);
        let left = self.leaving;
        cx.spawn(async move |this, cx| {
            let picture = match taken.await {
                Ok(picture) => cx.background_executor().spawn(render(picture)).await,
                Err(why) => {
                    tracing::debug!(%why, "no picture of a view");
                    None
                }
            };
            let _ = this.update(cx, |this, cx| {
                if this.epoch != epoch {
                    return;
                }
                this.picturing = false;
                let mut moving = look_again;
                if let Some((picture, print)) = picture {
                    // The first picture can't say whether the page moves:
                    // a second look will.
                    moving |= this.still.is_none() || print != this.print;
                    this.print = print;
                    if let Some(old) = this.still.replace(picture) {
                        cx.drop_image(old, None);
                    }
                }
                if left && std::mem::take(&mut this.leaving) {
                    tracing::debug!("a view's page goes back to being a picture");
                    this.live = false;
                }
                cx.notify();
                if std::mem::take(&mut this.picture_again) {
                    this.picture(cx);
                } else if moving && (!this.live || this.banded.get()) && this.in_sight() {
                    // It changes by itself: keep its picture roughly current.
                    this.settle = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(MOVING_EVERY).await;
                        let _ = this.update(cx, |this, cx| this.picture(cx));
                    }));
                }
            });
        })
        .detach();
    }

    fn in_sight(&self) -> bool {
        self.seen.get().is_some_and(Seen::in_sight)
    }

    // -- live, or a picture --------------------------------------------------------

    /// The pointer is on the view: show the real page. (It finds the
    /// pointer on it when it comes in: the system tells it.)
    fn come_alive(&mut self, cx: &mut Context<Self>) {
        self.leaving = false;
        if self
            .scrolled
            .get()
            .is_some_and(|at| at.elapsed() < QUIET_AFTER_SCROLL)
        {
            return;
        }
        if !self.live && self.opened {
            tracing::debug!("a view's page comes in under the pointer");
            self.live = true;
            cx.notify();
        }
        // Where the view reaches into the band its picture shows even now,
        // and the page changes under the pointer.
        if self.live && self.banded.get() {
            self.picture_soon(cx);
        }
    }

    /// The pointer left: back to a picture, once there is one of the page
    /// as it is now (so nothing is seen to change).
    fn leave(&mut self, cx: &mut Context<Self>) {
        if self.typing && self.host.as_ref().is_some_and(ViewHost::is_focused) {
            return;
        }
        if self.live && !self.leaving {
            self.leaving = true;
            self.look_again = true;
            self.picture(cx);
        }
    }

    /// Back to a picture this instant (the thread scrolls, a key went
    /// somewhere else): whatever picture there is, and a fresh one behind it.
    fn rest(&mut self, cx: &mut Context<Self>) {
        self.scrolled.set(Some(Instant::now()));
        if self.live {
            tracing::debug!("a view's page rests at once");
            self.live = false;
            self.leaving = false;
            self.look_again = true;
            if let Some(host) = &self.host {
                host.park();
            }
            cx.notify();
            self.picture(cx);
        }
    }

    /// A press on the picture: the page comes in under it and gets the
    /// press, as if it had been there.
    fn on_press(&mut self, _: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.opened {
            return;
        }
        let (Some(bounds), Some(clip), Some(host)) =
            (self.bounds.get(), self.clip.get(), self.host.clone())
        else {
            return;
        };
        // (A press can reach us with the page shown, too: in the band at
        // the bottom of the list, where only its picture is.)
        if !self.live {
            self.live = true;
            host.show_now(
                bounds,
                clip,
                window.scale_factor(),
                px(RADIUS),
                self.corners(),
            );
        }
        self.leaving = false;
        host.take_press();
        self.handed.set(true);
        cx.stop_propagation();
        cx.notify();
    }

    /// The pointer moved somewhere in the window while the page is in.
    fn on_move(&mut self, event: &MouseMoveEvent, on_page: bool, cx: &mut Context<Self>) {
        if self.handed.get() {
            // Still holding the press the page was handed.
            match (&self.host, event.pressed_button) {
                (Some(host), Some(MouseButton::Left)) => host.take_drag(),
                _ => self.handed.set(false),
            }
        } else if !on_page {
            // While the page is shown it takes every pointer event inside
            // it, so one that reaches the app is somewhere else.
            self.leave(cx);
        }
    }

    /// The release of a press that was handed to the page.
    fn on_release(&mut self) {
        if self.handed.replace(false)
            && let Some(host) = &self.host
        {
            host.take_release();
        }
    }

    fn corners(&self) -> Corners {
        Corners {
            top: self.flush_top.get(),
            bottom: true,
        }
    }

    /// The app's look changed (light to dark, another accent): tell the page.
    fn follow_theme(&mut self, cx: &mut Context<Self>) {
        if !self.opened {
            return;
        }
        let now = crate::view::theme(cx);
        if self.themed.as_ref() != Some(&now)
            && let Some(host) = &self.host
        {
            drop(host.run(&loft_view::theme_script(&now)));
            self.themed = Some(now);
            self.picture_soon(cx);
        }
    }
}

/// A page's picture as GPUI paints it, and a print of its pixels (two
/// pictures with the same print show the same thing). Reads every pixel:
/// for a background thread.
async fn render(picture: HostPicture) -> Option<(Arc<RenderImage>, u64)> {
    let (width, height) = (picture.width, picture.height);
    let bgra = picture.into_bgra()?;
    let print = print_of(&bgra);
    let pixels = image::RgbaImage::from_raw(width, height, bgra)?;
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        print,
    ))
}

/// A quick print of a picture's pixels, eight bytes at a step. Only ever
/// compared with the print of the picture before.
fn print_of(pixels: &[u8]) -> u64 {
    const MIX: u64 = 0x517c_c1b7_2722_0a95;
    let (words, rest) = pixels.as_chunks::<8>();
    let mut print = pixels.len() as u64;
    for word in words {
        print = (print.rotate_left(5) ^ u64::from_ne_bytes(*word)).wrapping_mul(MIX);
    }
    for byte in rest {
        print = (print.rotate_left(5) ^ u64::from(*byte)).wrapping_mul(MIX);
    }
    print
}

impl Render for EmbedView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.host.is_none() {
            self.wake(window, cx);
        }
        self.follow_theme(cx);
        let p = Palette::of(cx);
        let flush_top = self.flush_top.get();
        let body = match &self.still {
            Some(picture) => img(picture.clone())
                .size_full()
                .rounded_b(px(RADIUS))
                .when(flush_top, |i| i.rounded_t(px(RADIUS)))
                .into_any_element(),
            None => h_flex()
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
        let (bounds, clip, seen) = (self.bounds.clone(), self.clip.clone(), self.seen.clone());
        let (live, host, entity) = (self.live, self.host.clone(), cx.entity().downgrade());
        let banded = self.banded.clone();
        let scrolled = self.scrolled.clone();
        let handed = self.handed.clone();
        let band = window.rem_size() * BAND;
        div()
            .id(("view-embed", cx.entity_id()))
            .relative()
            .w_full()
            .h(px(self.height as f32))
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| this.come_alive(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_press))
            .child(body)
            .children(self.host.as_ref().map(|host| {
                div().absolute().top_0().left_0().size_full().child(
                    host.surface(live, px(RADIUS), self.corners())
                        .keep_clear_below(band),
                )
            }))
            .child(
                canvas(
                    move |area, window, _| {
                        bounds.set(Some(area));
                        seen.set(Some(Seen::now(window)));
                        area
                    },
                    move |_, area, window, _| {
                        let mask = window.content_mask().bounds;
                        banded.set(
                            area.bottom() > mask.bottom() - band && area.top() < mask.bottom(),
                        );
                        let cut = loft_browser::without_band(mask, band);
                        clip.set(Some(cut));
                        // What of the page can be in sight.
                        let shown = area.intersect(&cut);
                        // The thread scrolls (the wheel over anything but a
                        // shown page comes here first).
                        window.on_mouse_event(move |_: &ScrollEvent, phase, _, _| {
                            if phase == DispatchPhase::Capture {
                                scrolled.set(Some(Instant::now()));
                            }
                        });
                        // The pointer left the page; or it drags, holding a
                        // press the page was handed.
                        let moved = entity.clone();
                        let held = handed.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            let on_page = shown.contains(&event.position);
                            if phase == DispatchPhase::Capture && (held.get() || (live && !on_page))
                            {
                                let _ =
                                    moved.update(cx, |this, cx| this.on_move(event, on_page, cx));
                            }
                        });
                        let released = entity.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture
                                && event.button == MouseButton::Left
                                && handed.get()
                            {
                                let _ = released.update(cx, |this, _| this.on_release());
                            }
                        });
                        if !live {
                            return;
                        }
                        // A press elsewhere takes the keyboard back with it.
                        let (pressed, focused) = (entity.clone(), host.clone());
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture && !shown.contains(&event.position) {
                                if let Some(host) = &focused {
                                    host.release_focus();
                                }
                                let _ = pressed.update(cx, |this, cx| this.leave(cx));
                            }
                        });
                        // The thread scrolls, or a key opens something over
                        // it: only a picture can be scrolled and covered.
                        let scrolled = entity.clone();
                        window.on_mouse_event(move |_: &ScrollEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture {
                                let _ = scrolled.update(cx, |this, cx| this.rest(cx));
                            }
                        });
                        // (Not a key the page itself is getting: with the
                        // keyboard in the page, what reaches the app is a
                        // shortcut on its way to the Edit menu and back to
                        // the page: Cmd+A, Cmd+C, Cmd+V.)
                        let (typed, keyed) = (entity.clone(), host.clone());
                        window.on_key_event(move |_: &KeyDownEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture
                                && !keyed.as_ref().is_some_and(ViewHost::is_focused)
                            {
                                let _ = typed.update(cx, |this, cx| this.rest(cx));
                            }
                        });
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }
}
