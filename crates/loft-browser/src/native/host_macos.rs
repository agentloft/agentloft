//! The native side of a hosted page (see [`crate::host`]): a WKWebView in a
//! clipping container, with the one script world that can reach us.

use crate::host::{Corners, HostEvent, HostPicture};
use anyhow::Context as _;
use block2::RcBlock;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBitmapFormat, NSBitmapImageRep, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSImage, NSView,
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
/// The script world the bridge runs in, and the name it posts to.
const WORLD: &str = "agentloft-view";
const POST: &str = "agentloftView";
/// Where a page waits while it isn't shown: in the window (so WebKit keeps
/// it running and can picture it), far outside what the window shows.
const AWAY: f64 = -30_000.0;

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

pub(crate) struct NativeHost {
    // `webview` (wry) drops before `wk`, as in `NativeWebView`.
    webview: wry::WebView,
    wk: Retained<WKWebView>,
    /// Clips the page to what of it is in sight, and rounds its corners.
    container: Retained<NSView>,
    controller: Retained<WKUserContentController>,
    bridge_world: Retained<WKContentWorld>,
    page_world: Retained<WKContentWorld>,
    _handler: Retained<MessageHandler>,
    /// The page's size in points, shown or not.
    size: Cell<(f64, f64)>,
    /// What was last applied, to skip frames that change nothing.
    placed: Cell<Option<[f64; 10]>>,
    /// Whether the wheel is the page's own (it has more than it shows and
    /// was clicked into) rather than the thread's around it.
    scrolls: Cell<bool>,
    shared: Rc<Shared>,
    /// Paint generation for the surface element's drop guard.
    pub(crate) paint_token: Cell<u64>,
}

thread_local! {
    /// Every live host, for the wheel monitor.
    static HOSTS: RefCell<Vec<Weak<NativeHost>>> = const { RefCell::new(Vec::new()) };
    static MONITOR: Cell<bool> = const { Cell::new(false) };
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

        // SAFETY: main thread; plain AppKit view-hierarchy calls. The
        // container takes the web view's place under GPUI's content view.
        let container = unsafe {
            let parent = wk.superview().context("the web view has no parent")?;
            let container = NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(AWAY, AWAY), NSSize::new(size.0, size.1)),
            );
            container.setWantsLayer(true);
            container.setClipsToBounds(true);
            let layer: Option<Retained<AnyObject>> = msg_send![&*container, layer];
            if let Some(layer) = layer {
                let () = msg_send![&*layer, setMasksToBounds: true];
            }
            wk.removeFromSuperview();
            wk.setFrame(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(size.0, size.1),
            ));
            container.addSubview(&wk);
            parent.addSubview(&container);
            container
        };

        // SAFETY: main thread; shared singletons and the view's own
        // configuration.
        let (bridge_world, page_world, controller) = unsafe {
            (
                WKContentWorld::worldWithName(ns_string!("agentloft-view"), mtm),
                WKContentWorld::pageWorld(mtm),
                wk.configuration().userContentController(),
            )
        };
        debug_assert_eq!(WORLD, "agentloft-view");
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
            container,
            controller,
            bridge_world,
            page_world,
            _handler: handler,
            size: Cell::new(size),
            placed: Cell::new(None),
            scrolls: Cell::new(false),
            shared,
            paint_token: Cell::new(0),
        });
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
                    (Some(image), None) => pixels_of(image),
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

    /// Whether the wheel belongs to the page once it is clicked into.
    pub fn set_scrolls(&self, scrolls: bool) {
        self.scrolls.set(scrolls);
    }

    fn apply_away(&self) {
        let (w, h) = self.size.get();
        self.container.setFrame(NSRect::new(
            NSPoint::new(AWAY - w, AWAY - h),
            NSSize::new(w, h),
        ));
        self.wk
            .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
    }

    /// Take the page out of sight (it keeps running).
    pub fn park(&self) {
        if self.placed.take().is_some() {
            self.release_focus();
            self.apply_away();
        }
    }

    /// Show the page at `bounds`, as much of it as is inside `visible`
    /// (both in GPUI window coordinates: points, top-left origin).
    pub fn place(&self, bounds: [f64; 4], visible: [f64; 4], radius: f64, corners: Corners) {
        // SAFETY: main-thread view hierarchy read.
        let Some(parent) = (unsafe { self.container.superview() }) else {
            return;
        };
        let parent_height = parent.bounds().size.height;
        let [bx, by, bw, bh] = bounds;
        let [vx, vy, vw, vh] = visible;
        let mask = corners.mask(by, bh, vy, vh);
        let key = [
            bx,
            by,
            bw,
            bh,
            vx,
            vy,
            vw,
            vh,
            parent_height,
            radius + mask as f64 * 1e3,
        ];
        if self.placed.get() == Some(key) {
            return;
        }
        self.size.set((bw, bh));
        // AppKit's y axis points up in both GPUI's content view and ours.
        self.container.setFrame(NSRect::new(
            NSPoint::new(vx, parent_height - vy - vh),
            NSSize::new(vw, vh),
        ));
        self.wk.setFrame(NSRect::new(
            NSPoint::new(bx - vx, (vy + vh) - (by + bh)),
            NSSize::new(bw, bh),
        ));
        // SAFETY: CALayer properties of the container's own layer.
        unsafe {
            let layer: Option<Retained<AnyObject>> = msg_send![&*self.container, layer];
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

    /// The pointer has left the page. The page can't tell (it is about to
    /// be moved out from under the pointer, which is no event), and would
    /// go on showing whatever it showed under it: so it is told.
    pub fn pointer_left(&self) {
        let Some(window) = self.wk.window() else {
            return;
        };
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::MouseMoved,
            // Outside any window content.
            NSPoint::new(-1.0, -1.0),
            NSEventModifierFlags::empty(),
            objc2_foundation::NSProcessInfo::processInfo().systemUptime(),
            window.windowNumber(),
            None,
            0,
            0,
            0.0,
        );
        if let Some(event) = event {
            // On the next turn of the run loop, not now: the web view hands
            // a move it has no use for on to the app's own view, which is
            // in the middle of handling whatever brought us here.
            // SAFETY: a plain `NSObject` method; the event is retained by
            // the pending perform until it has been delivered.
            unsafe {
                let () = msg_send![
                    &*self.wk,
                    performSelector: objc2::sel!(mouseMoved:),
                    withObject: &*event,
                    afterDelay: 0.0f64,
                ];
            }
        }
    }

    /// A real click at `(x, y)` of the window (GPUI coordinates), wherever
    /// that lands: on the page if it is shown there, on the app if it is
    /// only pictured (dev hooks).
    pub fn click_in_window(&self, x: f64, y: f64) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        // SAFETY: main-thread view hierarchy read.
        let Some(parent) = (unsafe { self.container.superview() }) else {
            return;
        };
        let at = parent.convertPoint_toView(NSPoint::new(x, parent.bounds().size.height - y), None);
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

    /// The press being handled right now reached GPUI because the page was
    /// only a picture a moment ago. Now that it is there, it gets the press.
    pub fn take_press(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let Some(event) = app.currentEvent() else {
            return;
        };
        let kind = event.r#type();
        if kind == NSEventType::LeftMouseDown {
            let _ = self.webview.focus();
            self.wk.mouseDown(&event);
        }
    }

    /// A real click at `(x, y)` from the page's top left, queued for the
    /// app as the system queues one (dev hooks: this needs no leave from
    /// the system, and tests what a user's click does).
    pub fn click_at(&self, x: f64, y: f64) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        let (_, h) = self.size.get();
        // The web view's own y axis may point either way.
        let inside = if self.wk.isFlipped() {
            NSPoint::new(x, y)
        } else {
            NSPoint::new(x, h - y)
        };
        let at = self.wk.convertPoint_toView(inside, None);
        tracing::debug!(
            flipped = self.wk.isFlipped(),
            ?at,
            "a click is queued for a view's page"
        );
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
                // Queued, not sent: the app is in the middle of something
                // (us), and handles it when it is done.
                app.postEvent_atStart(&event, false);
            }
        }
    }

    /// A turn of the wheel over `(x, y)` from the page's top left, queued
    /// for the app as the system queues one (dev hooks).
    pub fn wheel_at(&self, x: f64, y: f64, lines: i32) {
        let (Some(window), Some(mtm)) = (self.wk.window(), MainThreadMarker::new()) else {
            return;
        };
        let (_, h) = self.size.get();
        let inside = if self.wk.isFlipped() {
            NSPoint::new(x, y)
        } else {
            NSPoint::new(x, h - y)
        };
        let in_window = self.wk.convertPoint_toView(inside, None);
        let on_screen = window.convertPointToScreen(in_window);
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
            // The window server says which window an event is for; a made
            // one has to say so itself.
            for field in [WINDOW_UNDER_POINTER, WINDOW_THAT_HANDLES] {
                CGEventSetIntegerValueField(wheel, field, window.windowNumber() as i64);
            }
            let event: Option<Retained<NSEvent>> =
                msg_send![objc2::class!(NSEvent), eventWithCGEvent: wheel];
            CFRelease(wheel.cast());
            tracing::debug!(
                made = event.is_some(),
                at = ?event.as_ref().map(|e| e.locationInWindow()),
                wanted = ?in_window,
                "a wheel event is queued for a view's page"
            );
            if let Some(event) = event {
                NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
            }
        }
    }

    /// The page's frame on screen (AppKit coordinates), if shown.
    fn screen_frame(&self) -> Option<NSRect> {
        if !self.is_shown() {
            return None;
        }
        let in_window = self
            .container
            .convertRect_toView(self.container.bounds(), None);
        Some(self.wk.window()?.convertRectToScreen(in_window))
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
            self.container.removeFromSuperview();
        }
    }
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
    fn CGEventCreateScrollWheelEvent(
        source: *const std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> *mut CGEvent;
    fn CGEventSetLocation(event: *mut CGEvent, location: NSPoint);
    fn CGEventSetIntegerValueField(event: *mut CGEvent, field: u32, value: i64);
}

/// `kCGMouseEventWindowUnderMousePointer` and
/// `kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent`.
const WINDOW_UNDER_POINTER: u32 = 91;
const WINDOW_THAT_HANDLES: u32 = 92;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const std::ffi::c_void);
}

/// One monitor for the app: a wheel over a shown page scrolls the thread
/// around it (as it does over any other row), unless the page has more than
/// it shows and was clicked into.
fn watch_the_wheel() {
    if MONITOR.with(|m| m.replace(true)) {
        return;
    }
    let pass = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes a live event for the length of the call.
        let event_ref = unsafe { event.as_ref() };
        // On screen, where both the event and the pages can be placed
        // whichever window the event names (a made one names none).
        let Some(mtm) = MainThreadMarker::new() else {
            return event.as_ptr();
        };
        let at = match event_ref.window(mtm) {
            Some(window) => window.convertPointToScreen(event_ref.locationInWindow()),
            None => event_ref.locationInWindow(),
        };
        let hit = HOSTS.with(|hosts| {
            hosts
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .find(|host| {
                    host.screen_frame().is_some_and(|frame| {
                        at.x >= frame.origin.x
                            && at.x <= frame.origin.x + frame.size.width
                            && at.y >= frame.origin.y
                            && at.y <= frame.origin.y + frame.size.height
                    })
                })
        });
        tracing::trace!(at = ?event_ref.locationInWindow(), on_a_page = hit.is_some(), "the wheel turned");
        let Some(host) = hit else {
            return event.as_ptr();
        };
        if host.scrolls.get() && host.is_focused() {
            return event.as_ptr();
        }
        // SAFETY: main-thread view hierarchy read.
        if let Some(parent) = unsafe { host.container.superview() } {
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

/// The picture's pixels as GPUI wants them (BGRA, rows top to bottom).
fn pixels_of(image: &NSImage) -> Result<HostPicture, String> {
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
    // Where red, green, blue and alpha sit in each pixel.
    let [r, g, b, a] = if format.contains(NSBitmapFormat::AlphaFirst) {
        [1, 2, 3, 0]
    } else {
        [0, 1, 2, 3]
    };
    let mut bgra = Vec::with_capacity(width * height * 4);
    // SAFETY: `bitmapData` points at `bytesPerRow * pixelsHigh` bytes owned
    // by `rep`, which outlives this read.
    let bytes = unsafe { std::slice::from_raw_parts(data, row * height) };
    for line in bytes.chunks_exact(row) {
        for pixel in line[..width * 4].as_chunks::<4>().0 {
            bgra.extend_from_slice(&[pixel[b], pixel[g], pixel[r], pixel[a]]);
        }
    }
    Ok(HostPicture {
        bgra,
        width: width as u32,
        height: height as u32,
    })
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
