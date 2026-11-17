//! The native side of a hosted page (see [`crate::host`]).
//!
//! Three views, from the window in: GPUI's content view; a [`ClipView`], as
//! big as what the list shows, which cuts the page off where the list ends;
//! and the WKWebView, as big as the page, placed inside the clip where its
//! row is. The web view is the one that moves, so AppKit and WebKit both
//! hear of every move: AppKit tells the page when it comes under the
//! pointer or is taken from under it, and WebKit keeps what depends on
//! where the page is in the right place.
//!
//! Two things a web view in someone else's window needs, each done the way
//! AppKit means it to be:
//!
//! - **The pointer.** WebKit sets the pointer for what is under it, but
//!   stands back while a cursor rectangle is in force, and GPUI keeps one
//!   over its whole window. AppKit asks the view under the pointer first
//!   (`cursorUpdate:`), and a view with nothing to say passes the question
//!   up, to GPUI in the end. The clip answers it: over the page, the
//!   pointer is the page's.
//! - **The wheel.** A web view keeps a wheel it has no use for. Each scroll
//!   gesture is given, as it begins, to the page or to the thread around
//!   it, and stays there until it ends (an event monitor does this).

use crate::host::{Corners, HostEvent, HostPicture};
use anyhow::Context as _;
use block2::RcBlock;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBitmapFormat, NSBitmapImageRep, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventPhase, NSEventType, NSImage, NSView,
};
use objc2_foundation::{NSError, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, ns_string};
use objc2_web_kit::{
    WKContentRuleList, WKContentRuleListStore, WKContentWorld, WKScriptMessage,
    WKScriptMessageHandler, WKSnapshotConfiguration, WKUserContentController, WKWebView,
};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder, WebViewExtMacOS as _};

/// The document every hosted page starts as: content is written into it.
const SHELL: &str =
    "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";
/// The name the bridge posts to, in its own script world.
const POST: &str = "agentloftView";

/// What the delegate callbacks share. Touched on the main thread only.
struct Shared {
    events: UnboundedSender<HostEvent>,
    ruled: Cell<bool>,
}

impl Shared {
    fn send(&self, event: HostEvent) {
        let _ = self.events.unbounded_send(event);
    }

    /// The empty document is in (again, if the page was made to load
    /// anew), and nothing can load that the rules don't allow.
    fn on_loaded(&self) {
        if self.ruled.get() {
            self.send(HostEvent::Ready);
        }
    }
}

struct HandlerIvars {
    shared: Rc<Shared>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements, and the class is
    // only ever used on the main thread (WebKit calls it there).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AgentloftViewMessageHandler"]
    #[ivars = HandlerIvars]
    struct MessageHandler;

    unsafe impl NSObjectProtocol for MessageHandler {}

    unsafe impl WKScriptMessageHandler for MessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: a property read on the live message WebKit passed.
            let body = unsafe { message.body() };
            if let Some(text) = body.downcast_ref::<NSString>() {
                self.ivars()
                    .shared
                    .send(HostEvent::Message(text.to_string()));
            }
        }
    }
);

impl MessageHandler {
    fn new(mtm: MainThreadMarker, shared: Rc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { shared });
        // SAFETY: `NSObject`'s designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY: `NSView` is made to be subclassed, and each override below
    // keeps what AppKit expects of it; used on the main thread only, like
    // every view.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "AgentloftViewClip"]
    struct ClipView;

    impl ClipView {
        /// Counted from the top left, like everything the app lays out.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// Only the page is here to be clicked: everywhere else this view
        /// covers belongs to the app under it.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            // SAFETY: `NSView`'s own answer, to the question as it was put.
            let hit: Option<Retained<NSView>> = unsafe { msg_send![super(self), hitTest: point] };
            let this: *const Self = self;
            hit.filter(|view| !std::ptr::eq(Retained::as_ptr(view).cast::<Self>(), this))
        }

        /// The pointer is over the page (nothing else in here can be under
        /// it) and AppKit asks what it should look like. A view with
        /// nothing to say passes that up to its superview, which here ends
        /// at GPUI's cursor rectangle; and while one of those is in force
        /// WebKit sets nothing. So this view answers, by saying nothing:
        /// the page sets the pointer for what is under it.
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {}
    }
);

impl ClipView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        let nowhere = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        // SAFETY: `NSView`'s designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: nowhere] }
    }
}

thread_local! {
    /// Every live host, for the wheel monitor.
    static HOSTS: RefCell<Vec<Weak<NativeHost>>> = const { RefCell::new(Vec::new()) };
    static MONITOR: Cell<bool> = const { Cell::new(false) };
    /// Who the scroll gesture under way belongs to: the page it began over
    /// (`true`) or the thread.
    static GESTURE: Cell<Option<bool>> = const { Cell::new(None) };
}

pub(crate) struct NativeHost {
    // `webview` (wry) drops before `wk`, as in `NativeWebView`.
    webview: wry::WebView,
    wk: Retained<WKWebView>,
    /// As big as what the list shows: the page is cut off where it ends.
    clip: Retained<ClipView>,
    controller: Retained<WKUserContentController>,
    bridge_world: Retained<WKContentWorld>,
    page_world: Retained<WKContentWorld>,
    _handler: Retained<MessageHandler>,
    /// The page's size in points, shown or not.
    size: Cell<(f64, f64)>,
    /// What was last applied, to skip frames that change nothing.
    placed: Cell<Option<[f64; 10]>>,
    /// Which ways the wheel has something to turn in the page where the
    /// pointer is: `[left, right, up, down]`, as the bridge says.
    reach: Cell<[bool; 4]>,
    shared: Rc<Shared>,
    /// Paint generation for the surface element's drop guard.
    pub(crate) paint_token: Cell<u64>,
}

impl NativeHost {
    /// A page at `origin` (an address made up for it), waiting out of
    /// sight. Nothing loads in it but scripts, styles and fonts from
    /// `allowed_hosts`.
    pub fn new(
        window: &gpui_kit::Window,
        origin: &str,
        allowed_hosts: &[&str],
        events: UnboundedSender<HostEvent>,
    ) -> anyhow::Result<Rc<Self>> {
        let mtm = MainThreadMarker::new().context("a view must be made on the main thread")?;
        let shared = Rc::new(Shared {
            events,
            ruled: Cell::new(false),
        });
        let home = format!("{}/", origin.trim_end_matches('/'));
        let size = (640.0, 160.0);

        let builder = WebViewBuilder::new()
            .with_bounds(wry::Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(size.0, size.1).into(),
            })
            .with_visible(true)
            .with_focused(false)
            .with_incognito(true)
            .with_devtools(cfg!(debug_assertions))
            // The page stays where it is: only its own document loads.
            .with_navigation_handler({
                let home = home.clone();
                move |url: String| url == home || url == "about:blank"
            })
            .with_on_page_load_handler({
                let shared = shared.clone();
                move |event, _url| {
                    if matches!(event, PageLoadEvent::Finished) {
                        shared.on_loaded();
                    }
                }
            })
            .with_new_window_req_handler({
                let shared = shared.clone();
                move |url, _features| {
                    shared.send(HostEvent::Open(url));
                    NewWindowResponse::Deny
                }
            });
        let webview = builder
            .build_as_child(window)
            .context("failed to create the view's WKWebView")?;
        let wk: Retained<WKWebView> = Retained::into_super(webview.webview());

        // `clip` takes the web view's place under GPUI's content view, with
        // nothing of the page in it yet.
        let clip = ClipView::new(mtm);
        clip.setWantsLayer(true);
        clip.setClipsToBounds(true);
        // SAFETY: main thread; plain AppKit view-hierarchy calls.
        unsafe {
            let parent = wk.superview().context("the web view has no parent")?;
            wk.removeFromSuperview();
            clip.addSubview(&wk);
            parent.addSubview(&clip);
        }
        // Round corners are the web view's own (the clip's edges are the
        // list's, which are straight).
        wk.setClipsToBounds(true);

        // SAFETY: main thread; shared singletons and the view's own
        // configuration.
        let (bridge_world, page_world, controller) = unsafe {
            (
                WKContentWorld::worldWithName(ns_string!("agentloft-view"), mtm),
                WKContentWorld::pageWorld(mtm),
                wk.configuration().userContentController(),
            )
        };
        let handler = MessageHandler::new(mtm, shared.clone());
        // SAFETY: main thread. Only scripts in the bridge's world get the
        // handler; the page's own scripts cannot post to it.
        unsafe {
            controller.addScriptMessageHandler_contentWorld_name(
                ProtocolObject::from_ref(&*handler),
                &bridge_world,
                &NSString::from_str(POST),
            );
        }

        let host = Rc::new(Self {
            webview,
            wk,
            clip,
            controller,
            bridge_world,
            page_world,
            _handler: handler,
            size: Cell::new(size),
            placed: Cell::new(None),
            reach: Cell::new([false; 4]),
            shared,
            paint_token: Cell::new(0),
        });
        host.apply_away();
        host.rule_then_load(mtm, allowed_hosts, &home)?;
        HOSTS.with(|hosts| {
            let mut hosts = hosts.borrow_mut();
            hosts.retain(|h| h.strong_count() > 0);
            hosts.push(Rc::downgrade(&host));
        });
        watch_the_wheel();
        Ok(host)
    }

    /// Put the network rules in place, and only then load the shell: a page
    /// never runs without them.
    fn rule_then_load(
        self: &Rc<Self>,
        mtm: MainThreadMarker,
        allowed_hosts: &[&str],
        home: &str,
    ) -> anyhow::Result<()> {
        let rules = rules(allowed_hosts);
        let id = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            rules.hash(&mut hasher);
            format!("agentloft-view-{:016x}", hasher.finish())
        };
        let host = Rc::downgrade(self);
        let home = home.to_owned();
        let compiled = RcBlock::new(move |list: *mut WKContentRuleList, error: *mut NSError| {
            let Some(host) = host.upgrade() else {
                return;
            };
            // SAFETY: valid-or-nil pointers for the length of the callback;
            // main thread.
            unsafe {
                let Some(list) = list.as_ref() else {
                    let why = error
                        .as_ref()
                        .map(|e| e.localizedDescription().to_string())
                        .unwrap_or_default();
                    tracing::warn!(%why, "a view's network rules didn't compile; it stays empty");
                    return;
                };
                host.controller.addContentRuleList(list);
                host.shared.ruled.set(true);
                let base = NSURL::URLWithString(&NSString::from_str(&home));
                host.wk
                    .loadHTMLString_baseURL(&NSString::from_str(SHELL), base.as_deref());
            }
        });
        // SAFETY: main thread; the store retains the block until it calls it.
        unsafe {
            let store = WKContentRuleListStore::defaultStore(mtm)
                .context("WebKit has no content rule store")?;
            store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                Some(&NSString::from_str(&id)),
                Some(&NSString::from_str(&rules)),
                Some(&compiled),
            );
        }
        Ok(())
    }

    /// Runs `body` as an async function body, in the bridge's world or the
    /// page's own.
    pub fn run(&self, body: &str, bridge: bool) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Cell::new(Some(tx));
        let done = RcBlock::new(move |_result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: a valid-or-nil pointer for the length of the callback.
            let outcome = match unsafe { error.as_ref() } {
                Some(error) => Err(error.localizedDescription().to_string()),
                None => Ok(()),
            };
            if let Some(tx) = tx.take() {
                let _ = tx.send(outcome);
            }
        });
        let world = if bridge {
            &self.bridge_world
        } else {
            &self.page_world
        };
        // SAFETY: main thread; nil arguments and frame mean "none, main
        // frame"; WebKit retains the block until it calls it.
        unsafe {
            self.wk
                .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(body),
                    None,
                    None,
                    world,
                    Some(&done),
                );
        }
        rx
    }

    /// A picture of the whole page as it is now, shown or not.
    pub fn snapshot(&self) -> oneshot::Receiver<Result<HostPicture, String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Cell::new(Some(tx));
        let done = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            // SAFETY: valid-or-nil pointers for the length of the callback.
            let outcome = unsafe {
                match (image.as_ref(), error.as_ref()) {
                    (_, Some(error)) => Err(error.localizedDescription().to_string()),
                    (Some(image), None) => pixels_of(image).or_else(|_| pixels_by_tiff(image)),
                    (None, None) => Err("WebKit returned no image".to_owned()),
                }
            };
            if let Some(tx) = tx.take() {
                let _ = tx.send(outcome);
            }
        });
        let mtm = self.wk.mtm();
        let (width, _) = self.size.get();
        // SAFETY: main thread; WebKit retains the configuration and block.
        unsafe {
            let config = WKSnapshotConfiguration::new(mtm);
            config.setAfterScreenUpdates(true);
            if width > 0.0 {
                config.setSnapshotWidth(Some(&NSNumber::new_f64(width)));
            }
            self.wk
                .takeSnapshotWithConfiguration_completionHandler(Some(&config), &done);
        }
        rx
    }

    /// The page's size in points. It lays out to the width whether it is
    /// shown or not.
    pub fn set_size(&self, width: f64, height: f64) {
        let size = (width.max(1.0), height.max(1.0));
        if self.size.get() == size {
            return;
        }
        self.size.set(size);
        if self.placed.get().is_none() {
            self.apply_away();
        }
    }

    /// Which ways the wheel has something to turn in the page where the
    /// pointer is: `[left, right, up, down]`.
    pub fn set_reach(&self, reach: [bool; 4]) {
        self.reach.set(reach);
    }

    /// The page at its size, beside what the clip shows: in the window (so
    /// WebKit keeps it running and can picture it), with none of it in
    /// sight or in reach.
    fn apply_away(&self) {
        let (w, h) = self.size.get();
        self.wk.setFrame(NSRect::new(
            NSPoint::new(-(w + 64.0), 0.0),
            NSSize::new(w, h),
        ));
    }

    /// Take the page out of sight. It keeps running, and keeps the
    /// keyboard if it had it: what is selected in it can still be copied,
    /// and what is typed still goes to its field. (If the pointer was on
    /// it, AppKit tells it the pointer no longer is.)
    pub fn park(&self) {
        if self.placed.take().is_some() {
            self.apply_away();
        }
    }

    /// Show the page at `bounds`, as much of it as is inside `clip` (both
    /// in GPUI window coordinates: points, from the top left).
    pub fn place(&self, bounds: [f64; 4], clip: [f64; 4], radius: f64, corners: Corners) {
        // SAFETY: main-thread view hierarchy read.
        let Some(parent) = (unsafe { self.clip.superview() }) else {
            return;
        };
        let parent_height = parent.bounds().size.height;
        let [bx, by, bw, bh] = bounds;
        let [cx, cy, cw, ch] = clip;
        let mask = corners.mask(self.wk.isFlipped());
        let key = [
            bx,
            by,
            bw,
            bh,
            cx,
            cy,
            cw,
            ch,
            parent_height,
            radius + mask as f64 * 1e3,
        ];
        if self.placed.get() == Some(key) {
            return;
        }
        self.size.set((bw, bh));
        // GPUI's content view may count y from its bottom; the clip counts
        // from its top, as GPUI's own coordinates do.
        let clip_y = if parent.isFlipped() {
            cy
        } else {
            parent_height - cy - ch
        };
        let clip_frame = NSRect::new(NSPoint::new(cx, clip_y), NSSize::new(cw, ch));
        if self.clip.frame() != clip_frame {
            self.clip.setFrame(clip_frame);
        }
        self.wk.setFrame(NSRect::new(
            NSPoint::new(bx - cx, by - cy),
            NSSize::new(bw, bh),
        ));
        // SAFETY: plain properties of the web view's own layer (it has
        // one: its window is layer-backed).
        unsafe {
            let layer: Option<Retained<AnyObject>> = msg_send![&*self.wk, layer];
            if let Some(layer) = layer {
                let () = msg_send![&*layer, setCornerRadius: radius];
                let () = msg_send![&*layer, setMaskedCorners: mask];
            }
        }
        self.placed.set(Some(key));
    }

    pub fn is_shown(&self) -> bool {
        self.placed.get().is_some()
    }

    /// What shows of the page, on screen (AppKit coordinates).
    fn screen_frame(&self) -> Option<NSRect> {
        if !self.is_shown() {
            return None;
        }
        let visible = self.wk.visibleRect();
        if visible.size.width < 1.0 || visible.size.height < 1.0 {
            return None;
        }
        let in_window = self.wk.convertRect_toView(visible, None);
        Some(self.wk.window()?.convertRectToScreen(in_window))
    }

    /// Whether the web content has AppKit key focus.
    pub fn is_focused(&self) -> bool {
        let Some(window) = self.wk.window() else {
            return false;
        };
        let Some(responder) = window.firstResponder() else {
            return false;
        };
        let responder: &AnyObject = &responder;
        responder
            .downcast_ref::<NSView>()
            .is_some_and(|view| view.isDescendantOf(&self.wk))
    }

    /// Hands key focus back to GPUI's view if the web content has it.
    pub fn release_focus(&self) {
        if self.is_focused() {
            let _ = self.webview.focus_parent();
        }
    }

    /// The press being handled right now reached GPUI because the page was
    /// only a picture a moment ago. Now that it is there, it gets the press.
    pub fn take_press(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
            return;
        };
        if event.r#type() == NSEventType::LeftMouseDown {
            let _ = self.webview.focus();
            self.wk.mouseDown(&event);
        }
    }

    /// The release of a press the page was handed ([`Self::take_press`]).
    /// AppKit sends a release to whoever got the press, which was the app.
    pub fn take_release(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
            return;
        };
        if event.r#type() == NSEventType::LeftMouseUp {
            self.wk.mouseUp(&event);
        }
    }

    /// The pointer moved with a press held that the page was handed: the
    /// drag is the page's too (it selects, or moves a slider).
    pub fn take_drag(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
            return;
        };
        if event.r#type() == NSEventType::LeftMouseDragged {
            self.wk.mouseDragged(&event);
        }
    }

    // -- dev hooks: input as the system sends it ------------------------------

    /// `(x, y)` from the page's top left, in its window.
    fn in_window(&self, x: f64, y: f64) -> NSPoint {
        let (_, h) = self.size.get();
        // The web view's own y axis may point either way.
        let inside = if self.wk.isFlipped() {
            NSPoint::new(x, y)
        } else {
            NSPoint::new(x, h - y)
        };
        self.wk.convertPoint_toView(inside, None)
    }

    /// Queue a press and a release at `at` (window coordinates) for the app
    /// as the system queues them. (Queued, not sent: the app is in the
    /// middle of something, the caller, and handles them when it is done.)
    fn queue_click(&self, at: NSPoint) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        for kind in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                at,
                NSEventModifierFlags::empty(),
                objc2_foundation::NSProcessInfo::processInfo().systemUptime(),
                window.windowNumber(),
                None,
                0,
                1,
                1.0,
            );
            if let Some(event) = event {
                app.postEvent_atStart(&event, false);
            }
        }
    }

    /// A real click at `(x, y)` from the page's top left.
    pub fn click_at(&self, x: f64, y: f64) {
        self.queue_click(self.in_window(x, y));
    }

    /// A real click at `(x, y)` of the window (GPUI coordinates), wherever
    /// that lands: on the page if it is shown there, on the app if it is
    /// only pictured.
    pub fn click_in_window(&self, x: f64, y: f64) {
        // SAFETY: main-thread view hierarchy read.
        let Some(parent) = (unsafe { self.clip.superview() }) else {
            return;
        };
        let at = parent.convertPoint_toView(NSPoint::new(x, parent.bounds().size.height - y), None);
        self.queue_click(at);
    }

    /// Keys as the system would send them, queued for the app: each of
    /// `keys` pressed and released, with Command held if `command` (letters
    /// and digits only).
    pub fn type_keys(&self, keys: &str, command: bool) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let flags = if command {
            NSEventModifierFlags::Command
        } else {
            NSEventModifierFlags::empty()
        };
        for key in keys.chars() {
            let Some(code) = key_code(key) else {
                continue;
            };
            let text = NSString::from_str(&key.to_string());
            for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
                let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                    kind,
                    NSPoint::new(0.0, 0.0),
                    flags,
                    objc2_foundation::NSProcessInfo::processInfo().systemUptime(),
                    window.windowNumber(),
                    None,
                    &text,
                    &text,
                    false,
                    code,
                );
                if let Some(event) = event {
                    app.postEvent_atStart(&event, false);
                }
            }
        }
    }

    /// A turn of the wheel over `(x, y)` from the page's top left, queued
    /// for the app as the system queues one.
    pub fn wheel_at(&self, x: f64, y: f64, lines: i32) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        let on_screen = window.convertPointToScreen(self.in_window(x, y));
        // Quartz counts y down from the top of the main display.
        let top = objc2_app_kit::NSScreen::mainScreen(mtm)
            .map_or(0.0, |screen| screen.frame().size.height);
        // SAFETY: plain Core Graphics calls; the event is released below,
        // after AppKit has made its own from it.
        unsafe {
            let wheel = CGEventCreateScrollWheelEvent(std::ptr::null(), 1, 1, lines);
            if wheel.is_null() {
                return;
            }
            CGEventSetLocation(wheel, NSPoint::new(on_screen.x, top - on_screen.y));
            let event: Option<Retained<NSEvent>> =
                msg_send![objc2::class!(NSEvent), eventWithCGEvent: wheel];
            CFRelease(wheel.cast());
            if let Some(event) = event {
                NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
            }
        }
    }
}

impl Drop for NativeHost {
    fn drop(&mut self) {
        self.release_focus();
        // SAFETY: main thread (the host is `!Send`); undoes `new`.
        unsafe {
            self.controller
                .removeScriptMessageHandlerForName_contentWorld(
                    &NSString::from_str(POST),
                    &self.bridge_world,
                );
            self.clip.removeFromSuperview();
        }
    }
}

fn contains(frame: NSRect, at: NSPoint) -> bool {
    at.x >= frame.origin.x
        && at.x <= frame.origin.x + frame.size.width
        && at.y >= frame.origin.y
        && at.y <= frame.origin.y + frame.size.height
}

/// One monitor for the app, for the wheel over a shown page.
///
/// A scroll gesture belongs to whoever it began over, to its end and
/// through its momentum, as with any scroll view inside another. Over a
/// page it begins as the page's where the page has something to turn that
/// way: sideways always (the thread only goes up and down), up and down
/// once the page was clicked into (or scrolling the thread past a view
/// would catch on it). Otherwise it is the thread's, and is handed to it.
fn watch_the_wheel() {
    if MONITOR.with(|m| m.replace(true)) {
        return;
    }
    let pass = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes a live event for the length of the call.
        let event_ref = unsafe { event.as_ref() };
        let Some(mtm) = MainThreadMarker::new() else {
            return event.as_ptr();
        };
        // On screen, where both the event and the pages can be placed
        // whichever window the event names (a made one names none).
        let at = match event_ref.window(mtm) {
            Some(window) => window.convertPointToScreen(event_ref.locationInWindow()),
            None => event_ref.locationInWindow(),
        };
        let hit = HOSTS.with(|hosts| {
            hosts
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .find(|host| host.screen_frame().is_some_and(|frame| contains(frame, at)))
        });
        let (phase, momentum) = (event_ref.phase(), event_ref.momentumPhase());
        // A wheel that clicks has no phases: each click is its own gesture.
        let begins = phase.contains(NSEventPhase::Began)
            || phase.contains(NSEventPhase::MayBegin)
            || (phase.is_empty() && momentum.is_empty());
        let Some(host) = hit else {
            if begins {
                GESTURE.with(|g| g.set(None));
            }
            return event.as_ptr();
        };
        let for_the_page = if begins {
            let (dx, dy) = (event_ref.scrollingDeltaX(), event_ref.scrollingDeltaY());
            let [left, right, up, down] = host.reach.get();
            let wanted = if dx.abs() > dy.abs() {
                (dx > 0.0 && left) || (dx < 0.0 && right)
            } else if dy != 0.0 {
                host.is_focused() && ((dy > 0.0 && up) || (dy < 0.0 && down))
            } else {
                // Fingers down, nothing turned yet: whoever can take it.
                host.is_focused() && (up || down)
            };
            GESTURE.with(|g| g.set(Some(wanted)));
            wanted
        } else {
            GESTURE.with(Cell::get).unwrap_or(false)
        };
        if for_the_page {
            // The system takes it there itself, unless the event names no
            // window (one made by a dev hook), which nobody would get.
            if event_ref.window(mtm).is_some() {
                return event.as_ptr();
            }
            tracing::trace!("the wheel over a view's page goes to the page");
            host.wk.scrollWheel(event_ref);
            return std::ptr::null_mut();
        }
        // SAFETY: main-thread view hierarchy read.
        if let Some(parent) = unsafe { host.clip.superview() } {
            tracing::trace!("the wheel over a view's page goes to the thread");
            host.shared.send(HostEvent::Wheel);
            parent.scrollWheel(event_ref);
        }
        std::ptr::null_mut()
    });
    // SAFETY: the block returns the event it was given or null; AppKit
    // keeps the monitor for the life of the app, which is what is wanted.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::ScrollWheel, &pass)
    };
    std::mem::forget(monitor);
}

// -- pictures ---------------------------------------------------------------

/// A Quartz image, as far as we need to know it.
#[repr(C)]
struct CGImage {
    _opaque: [u8; 0],
}

// SAFETY: the encoding of `CGImageRef`'s pointee, as AppKit declares it.
unsafe impl objc2::encode::RefEncode for CGImage {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGImage", &[]));
}

/// A Quartz event, as far as we need to know it.
#[repr(C)]
struct CGEvent {
    _opaque: [u8; 0],
}

// SAFETY: the encoding of `CGEventRef`'s pointee, as AppKit declares it.
unsafe impl objc2::encode::RefEncode for CGEvent {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("__CGEvent", &[]));
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: *mut CGImage) -> usize;
    fn CGImageGetHeight(image: *mut CGImage) -> usize;
    fn CGImageGetBytesPerRow(image: *mut CGImage) -> usize;
    fn CGImageGetBitsPerPixel(image: *mut CGImage) -> usize;
    fn CGImageGetBitmapInfo(image: *mut CGImage) -> u32;
    fn CGImageGetDataProvider(image: *mut CGImage) -> *mut std::ffi::c_void;
    fn CGImageRetain(image: *mut CGImage) -> *mut CGImage;
    fn CGImageRelease(image: *mut CGImage);
    fn CGDataProviderCopyData(provider: *mut std::ffi::c_void) -> *const std::ffi::c_void;
    fn CGEventCreateScrollWheelEvent(
        source: *const std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> *mut CGEvent;
    fn CGEventSetLocation(event: *mut CGEvent, location: NSPoint);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const std::ffi::c_void) -> *const u8;
    fn CFDataGetLength(data: *const std::ffi::c_void) -> isize;
    fn CFRelease(object: *const std::ffi::c_void);
}

/// A page's picture as Quartz made it, kept until its pixels are read.
/// Reading them copies every one, so that is left to whoever wants them, on
/// a thread of their choosing: a Quartz image never changes once made, and
/// may be read and let go from any thread.
struct QuartzImage(*mut CGImage);

// SAFETY: see the type's note; we hold our own reference to the image.
unsafe impl Send for QuartzImage {}

impl QuartzImage {
    /// The pixels as GPUI draws them, given what the image said of itself.
    fn read(&self, row: usize, order: [u8; 4], width: usize, height: usize) -> Option<Vec<u8>> {
        // SAFETY: the image is alive (we hold a reference); the provider
        // belongs to it; the data copied from it is ours to release, and
        // is only read before that.
        unsafe {
            let provider = CGImageGetDataProvider(self.0);
            if provider.is_null() {
                return None;
            }
            let data = CGDataProviderCopyData(provider);
            if data.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(
                CFDataGetBytePtr(data),
                CFDataGetLength(data).max(0) as usize,
            );
            let pixels = crate::host::packed_bgra(bytes, row, order, width, height);
            CFRelease(data);
            pixels
        }
    }
}

impl Drop for QuartzImage {
    fn drop(&mut self) {
        // SAFETY: ours since `CGImageRetain`.
        unsafe { CGImageRelease(self.0) }
    }
}

/// The picture's pixels straight from its backing image, with no encoding
/// in between: what the image is like is read here, its pixels later.
fn pixels_of(image: &NSImage) -> Result<HostPicture, String> {
    // SAFETY: an `NSImage` method with nil arguments ("as it is"). The
    // image it returns belongs to `image`: we take a reference of our own
    // before this returns.
    unsafe {
        let cg: *mut CGImage = msg_send![
            image,
            CGImageForProposedRect: std::ptr::null_mut::<NSRect>(),
            context: Option::<&objc2_app_kit::NSGraphicsContext>::None,
            hints: Option::<&objc2_foundation::NSDictionary>::None,
        ];
        if cg.is_null() || CGImageGetBitsPerPixel(cg) != 32 {
            return Err("the picture has no 32-bit image behind it".into());
        }
        let (width, height, row) = (
            CGImageGetWidth(cg),
            CGImageGetHeight(cg),
            CGImageGetBytesPerRow(cg),
        );
        if width == 0 || height == 0 || row < width * 4 {
            return Err("the picture's image can't be read".into());
        }
        let info = CGImageGetBitmapInfo(cg);
        // `kCGBitmapByteOrder32Little`, and alpha (or padding) first.
        let (little, alpha_first) = (info & 0x7000 == 0x2000, matches!(info & 0x1f, 2 | 4 | 6));
        // Where blue, green, red and alpha sit in each pixel.
        let order: [u8; 4] = match (little, alpha_first) {
            (true, true) => [0, 1, 2, 3],   // B G R A
            (false, false) => [2, 1, 0, 3], // R G B A
            (false, true) => [3, 2, 1, 0],  // A R G B
            (true, false) => [1, 2, 3, 0],  // A B G R
        };
        let kept = QuartzImage(CGImageRetain(cg));
        Ok(HostPicture::new(width as u32, height as u32, move || {
            kept.read(row, order, width, height)
        }))
    }
}

/// The slower way to the same pixels, for an image the fast way can't read.
fn pixels_by_tiff(image: &NSImage) -> Result<HostPicture, String> {
    let tiff = image
        .TIFFRepresentation()
        .ok_or("could not read the picture")?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("could not decode the picture")?;
    let (width, height) = (
        rep.pixelsWide().max(0) as usize,
        rep.pixelsHigh().max(0) as usize,
    );
    let (row, format) = (rep.bytesPerRow().max(0) as usize, rep.bitmapFormat());
    let data = rep.bitmapData();
    if data.is_null()
        || rep.isPlanar()
        || rep.bitsPerPixel() != 32
        || rep.samplesPerPixel() != 4
        || format.contains(NSBitmapFormat::FloatingPointSamples)
        || width == 0
        || height == 0
        || row < width * 4
    {
        return Err("the picture is in a format we don't read".into());
    }
    let order: [u8; 4] = if format.contains(NSBitmapFormat::AlphaFirst) {
        [3, 2, 1, 0]
    } else {
        [2, 1, 0, 3]
    };
    // SAFETY: `bitmapData` points at `bytesPerRow * pixelsHigh` bytes owned
    // by `rep`, which outlives this copy.
    let bytes = unsafe { std::slice::from_raw_parts(data, row * height) }.to_vec();
    Ok(HostPicture::new(width as u32, height as u32, move || {
        crate::host::packed_bgra(&bytes, row, order, width, height)
    }))
}

/// The virtual key code of a letter or digit on an ANSI keyboard.
fn key_code(key: char) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    match key.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(LETTERS[c as usize - 'a' as usize]),
        c @ '0'..='9' => Some(DIGITS[c as usize - '0' as usize]),
        _ => None,
    }
}

/// WebKit content rules: nothing loads but scripts, styles and fonts from
/// `hosts`. (One rule a host: WebKit's filters have no `a|b`.)
fn rules(hosts: &[&str]) -> String {
    let mut rules = vec![serde_json::json!({
        "trigger": { "url-filter": ".*" },
        "action": { "type": "block" },
    })];
    rules.extend(hosts.iter().map(|host| {
        serde_json::json!({
            "trigger": {
                "url-filter": format!("^https://{}/", host.replace('.', "\\.")),
                "resource-type": ["script", "style-sheet", "font"],
            },
            "action": { "type": "ignore-previous-rules" },
        })
    }));
    serde_json::Value::Array(rules).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_block_everything_but_the_listed_hosts() {
        let rules: serde_json::Value =
            serde_json::from_str(&rules(&["cdn.jsdelivr.net", "esm.sh"])).unwrap();
        assert_eq!(rules[0]["action"]["type"], "block");
        assert_eq!(rules[0]["trigger"]["url-filter"], ".*");
        assert_eq!(
            rules[1]["trigger"]["url-filter"],
            "^https://cdn\\.jsdelivr\\.net/"
        );
        assert_eq!(rules[2]["trigger"]["url-filter"], "^https://esm\\.sh/");
        assert_eq!(rules[2]["action"]["type"], "ignore-previous-rules");
        assert_eq!(rules.as_array().unwrap().len(), 3);
    }
}
