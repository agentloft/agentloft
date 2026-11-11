//! WKWebView through wry, plus the few WebKit calls wry does not wrap:
//! `callAsyncJavaScript` (awaits promises, runs in content worlds),
//! `takeSnapshotWithConfiguration`, and state reads (URL, title, progress).

use super::{JsWorld, LoadMark, LoadProgress, NativeEvent, NavShared, PageState, Screenshot};
use anyhow::Context as _;
use block2::RcBlock;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSView};
use objc2_foundation::{
    NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString, ns_string,
};
use objc2_web_kit::{WKContentWorld, WKSnapshotConfiguration, WKWebView};
use std::cell::Cell;
use std::rc::Rc;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder, WebViewExtMacOS as _};

/// `_WKMediaAudioMuted`, from WebKit's `_WKMediaMutedState` flags.
const WK_MEDIA_AUDIO_MUTED: usize = 1 << 0;

/// Safari's user agent: the bare WKWebView one lacks `Version/… Safari/…`
/// and some sites serve degraded pages to it.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// Size used until the view is first painted, so pages loaded while the
/// panel is hidden still lay out like a normal window.
const INITIAL_SIZE: (f64, f64) = (1024.0, 768.0);

pub(crate) struct NativeWebView {
    // Field order matters: `webview` (wry) must drop before `wk` so wry's
    // teardown runs while our extra reference still exists.
    webview: wry::WebView,
    wk: Retained<WKWebView>,
    agent_world: Retained<WKContentWorld>,
    page_world: Retained<WKContentWorld>,
    nav: Rc<NavShared>,
    hidden: Cell<bool>,
    /// `[x, y, w, h, parent_height]` last applied, to skip no-op frames.
    last_frame: Cell<Option<[f64; 5]>>,
    /// The page zoom last applied (1.0 = 100%).
    zoom: Cell<f64>,
    /// Paint generation for the surface element's drop guard.
    pub(crate) paint_token: Cell<u64>,
}

impl NativeWebView {
    /// Creates the WKWebView as a hidden child of the window's content view.
    /// It never takes key focus or activates the app on its own.
    pub fn new(
        window: &gpui_kit::Window,
        events: UnboundedSender<NativeEvent>,
    ) -> anyhow::Result<Self> {
        let mtm = MainThreadMarker::new()
            .context("the browser view must be created on the main thread")?;
        let nav = Rc::new(NavShared::new(events));

        let builder = WebViewBuilder::new()
            .with_bounds(wry::Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(INITIAL_SIZE.0, INITIAL_SIZE.1).into(),
            })
            .with_visible(false)
            .with_focused(false)
            .with_devtools(true)
            .with_user_agent(USER_AGENT)
            .with_back_forward_navigation_gestures(true)
            .with_navigation_handler({
                let nav = nav.clone();
                move |url: String| nav.allow_navigation(&url)
            })
            .with_on_page_load_handler({
                let nav = nav.clone();
                move |event, _url| match event {
                    PageLoadEvent::Started => nav.on_load_started(),
                    PageLoadEvent::Finished => nav.on_load_finished(),
                }
            })
            .with_document_title_changed_handler({
                let nav = nav.clone();
                move |_title| nav.send(NativeEvent::TitleChanged)
            })
            .with_new_window_req_handler({
                let nav = nav.clone();
                move |url, _features| {
                    nav.send(NativeEvent::OpenInPlace(url));
                    NewWindowResponse::Deny
                }
            });

        // GPUI's `Window` hands out its content NSView as the AppKit window
        // handle; wry adds the WKWebView as a subview of it.
        let webview = builder
            .build_as_child(window)
            .context("failed to create the WKWebView")?;
        let wk = Retained::into_super(webview.webview());

        // SAFETY: main thread (checked above); these return shared singletons.
        let (agent_world, page_world) = unsafe {
            (
                WKContentWorld::worldWithName(ns_string!("agentloft"), mtm),
                WKContentWorld::pageWorld(mtm),
            )
        };

        Ok(Self {
            webview,
            wk,
            agent_world,
            page_world,
            nav,
            hidden: Cell::new(true),
            last_frame: Cell::new(None),
            zoom: Cell::new(1.0),
            paint_token: Cell::new(0),
        })
    }

    pub fn load(&self, url: &str) -> anyhow::Result<()> {
        self.nav.permit_host_navigation(url);
        self.webview.load_url(url).context("load_url failed")
    }

    pub fn go_back(&self) {
        self.permit_history_item(-1);
        let _ = self.webview.go_back();
    }

    pub fn go_forward(&self) {
        self.permit_history_item(1);
        let _ = self.webview.go_forward();
    }

    pub fn reload(&self) {
        if let Some(url) = self.state().url {
            self.nav.permit_host_navigation(&url);
        }
        let _ = self.webview.reload();
    }

    /// Reload, revalidating everything against the server (skips caches).
    pub fn hard_reload(&self) {
        if let Some(url) = self.state().url {
            self.nav.permit_host_navigation(&url);
        }
        // SAFETY: plain WKWebView method on the main thread.
        unsafe { self.wk.reloadFromOrigin() };
    }

    /// Show WebKit's Web Inspector for the page, in a window of its own.
    /// (Docked, WebKit lays itself out inside our window and resizes the
    /// page view to fill it, with no way back.)
    pub fn open_devtools(&self) {
        self.webview.open_devtools();
        self.detach_devtools();
    }

    /// If the inspector is docked in our window, move it to its own window;
    /// returns whether it was docked. (It docks a moment after `show`, so
    /// the caller retries for a little while.)
    pub fn detach_devtools(&self) -> bool {
        // SAFETY: `_inspector` is WebKit's own SPI (what wry calls); each
        // selector is checked before it's sent.
        unsafe {
            let inspector: Option<Retained<AnyObject>> = objc2::msg_send![&*self.wk, _inspector];
            let Some(inspector) = inspector else {
                return false;
            };
            let responds = |sel: objc2::runtime::Sel| -> bool {
                objc2::msg_send![&*inspector, respondsToSelector: sel]
            };
            if !responds(objc2::sel!(inspectorWebView)) || !responds(objc2::sel!(detach)) {
                return false;
            }
            // Docked = its view lives in the same window as the page.
            let view: Option<Retained<NSView>> = objc2::msg_send![&*inspector, inspectorWebView];
            let docked = match (view.and_then(|v| v.window()), self.wk.window()) {
                (Some(a), Some(b)) => Retained::as_ptr(&a) == Retained::as_ptr(&b),
                _ => false,
            };
            if docked {
                let () = objc2::msg_send![&*inspector, detach];
            }
            docked
        }
    }

    pub fn close_devtools(&self) {
        self.webview.close_devtools();
    }

    pub fn devtools_open(&self) -> bool {
        self.webview.is_devtools_open()
    }

    /// Forget the browser's cookies (and other site storage), or its cache.
    pub fn clear_data(&self, what: super::SiteData) {
        use objc2_foundation::{NSDate, NSSet};
        use objc2_web_kit::{
            WKWebsiteDataTypeCookies, WKWebsiteDataTypeDiskCache,
            WKWebsiteDataTypeIndexedDBDatabases, WKWebsiteDataTypeLocalStorage,
            WKWebsiteDataTypeMemoryCache, WKWebsiteDataTypeSessionStorage,
        };
        // SAFETY: main-thread WebKit calls; the statics are WebKit constants.
        unsafe {
            let types: objc2::rc::Retained<NSSet<NSString>> = match what {
                super::SiteData::Cookies => NSSet::from_slice(&[
                    WKWebsiteDataTypeCookies,
                    WKWebsiteDataTypeLocalStorage,
                    WKWebsiteDataTypeSessionStorage,
                    WKWebsiteDataTypeIndexedDBDatabases,
                ]),
                super::SiteData::Cache => {
                    NSSet::from_slice(&[WKWebsiteDataTypeDiskCache, WKWebsiteDataTypeMemoryCache])
                }
            };
            let store = self.wk.configuration().websiteDataStore();
            let done = RcBlock::new(|| {});
            store.removeDataOfTypes_modifiedSince_completionHandler(
                &types,
                &NSDate::distantPast(),
                &done,
            );
        }
    }

    pub fn stop(&self) {
        // SAFETY: plain WKWebView method on the main thread.
        unsafe { self.wk.stopLoading() };
    }

    /// History navigation is host-initiated, so a `file:` entry may load.
    fn permit_history_item(&self, offset: isize) {
        // SAFETY: main-thread reads of the back/forward list.
        let url = unsafe {
            let list = self.wk.backForwardList();
            list.itemAtIndex(offset)
                .and_then(|item| item.URL().absoluteString())
        };
        if let Some(url) = url {
            self.nav.permit_host_navigation(&url.to_string());
        }
    }

    pub fn state(&self) -> PageState {
        // SAFETY: KVO-compliant property reads on the main thread.
        unsafe {
            PageState {
                url: self
                    .wk
                    .URL()
                    .and_then(|u| u.absoluteString())
                    .map(|s| s.to_string()),
                title: self.wk.title().map(|t| t.to_string()),
                loading: self.wk.isLoading(),
                progress: self.wk.estimatedProgress(),
                can_go_back: self.wk.canGoBack(),
                can_go_forward: self.wk.canGoForward(),
                playing_audio: self.playing_audio(),
                muted: self.muted(),
            }
        }
    }

    /// Mute or unmute the page's sound.
    pub fn set_muted(&self, muted: bool) {
        // SAFETY: `_setPageMuted:` is WebKit's own SPI (Safari's tab mute),
        // taking `_WKMediaMutedState` flags; checked before it's sent.
        unsafe {
            let sel = objc2::sel!(_setPageMuted:);
            let responds: bool = objc2::msg_send![&*self.wk, respondsToSelector: sel];
            if responds {
                let flags: usize = if muted { WK_MEDIA_AUDIO_MUTED } else { 0 };
                let () = objc2::msg_send![&*self.wk, _setPageMuted: flags];
            }
        }
    }

    /// Whether the page's sound is muted.
    fn muted(&self) -> bool {
        // SAFETY: as `set_muted`: WebKit SPI, checked before it's sent.
        unsafe {
            let sel = objc2::sel!(_mediaMutedState);
            let responds: bool = objc2::msg_send![&*self.wk, respondsToSelector: sel];
            if !responds {
                return false;
            }
            let flags: usize = objc2::msg_send![&*self.wk, _mediaMutedState];
            flags & WK_MEDIA_AUDIO_MUTED != 0
        }
    }

    /// Whether the page is making sound right now.
    fn playing_audio(&self) -> bool {
        // SAFETY: `_isPlayingAudio` is WebKit's own SPI (a BOOL property,
        // what Safari's tab speaker reads); checked before it's sent.
        unsafe {
            let sel = objc2::sel!(_isPlayingAudio);
            let responds: bool = objc2::msg_send![&*self.wk, respondsToSelector: sel];
            responds && objc2::msg_send![&*self.wk, _isPlayingAudio]
        }
    }

    pub fn load_mark(&self) -> LoadMark {
        self.nav.mark()
    }

    pub fn load_progress(&self, mark: LoadMark) -> LoadProgress {
        // SAFETY: property read on the main thread.
        let loading = unsafe { self.wk.isLoading() };
        self.nav.progress(mark, loading)
    }

    /// Positions the view at `(x, y, w, h)` in GPUI window coordinates
    /// (logical points, top-left origin).
    pub fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
        // SAFETY: main-thread view hierarchy read.
        let Some(parent) = (unsafe { self.wk.superview() }) else {
            return;
        };
        let parent_bounds = parent.bounds();
        // GPUI's content view is not flipped: AppKit's y axis points up.
        let origin_y = if parent.isFlipped() {
            y
        } else {
            parent_bounds.size.height - y - h
        };
        let rect = NSRect::new(NSPoint::new(x, origin_y), NSSize::new(w, h));
        // Skip only if the view is really there: WebKit moves it itself
        // while its inspector is docked.
        let key = [x, y, w, h, parent_bounds.size.height];
        if self.last_frame.get() == Some(key) && self.wk.frame() == rect {
            return;
        }
        self.wk.setFrame(rect);
        self.last_frame.set(Some(key));
    }

    /// Scales the page: CSS pixels per point. Below 1.0 a page lays out
    /// wider than the view (device emulation, the mini preview).
    pub fn set_page_zoom(&self, zoom: f64) {
        let zoom = zoom.clamp(0.1, 3.0);
        if (self.zoom.get() - zoom).abs() < 1e-3 {
            return;
        }
        // SAFETY: main-thread property write (macOS 11+).
        unsafe { self.wk.setPageZoom(zoom) };
        self.zoom.set(zoom);
    }

    /// A custom user agent (`None` = WebKit's own); applies to new loads.
    pub fn set_user_agent(&self, agent: Option<&str>) {
        let agent = agent.map(NSString::from_str);
        // SAFETY: main-thread property write.
        unsafe { self.wk.setCustomUserAgent(agent.as_deref()) };
    }

    pub fn set_hidden(&self, hidden: bool) {
        if self.hidden.get() == hidden {
            return;
        }
        if hidden {
            // A hidden first responder would leave the window with no key
            // view, and GPUI would stop receiving keystrokes.
            self.release_focus();
        }
        self.wk.setHidden(hidden);
        self.hidden.set(hidden);
    }

    /// Whether the web content currently has AppKit key focus.
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

    /// Gives the web content key focus (explicit user intent only).
    pub fn focus(&self) {
        if !self.hidden.get() && !self.is_focused() {
            let _ = self.webview.focus();
        }
    }

    /// Hands key focus back to GPUI's view if the web content has it.
    pub fn release_focus(&self) {
        if self.is_focused() {
            let _ = self.webview.focus_parent();
        }
    }

    /// Runs `body` as an async function body in `world`. Resolves with the
    /// returned string, `None` for `undefined`/`null`, or the JS error text.
    pub fn call_js(
        &self,
        body: &str,
        world: JsWorld,
    ) -> oneshot::Receiver<Result<Option<String>, String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Cell::new(Some(tx));
        let handler = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: WebKit passes valid (or nil) object pointers that live
            // for the duration of the callback.
            let outcome = unsafe { js_outcome(result.as_ref(), error.as_ref()) };
            if let Some(tx) = tx.take() {
                let _ = tx.send(outcome);
            }
        });
        let world = match world {
            JsWorld::Agent => &self.agent_world,
            JsWorld::Page => &self.page_world,
        };
        // SAFETY: main thread; nil arguments and frame mean "no arguments,
        // main frame"; the block is retained by WebKit until it is called.
        unsafe {
            self.wk
                .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(body),
                    None,
                    None,
                    world,
                    Some(&handler),
                );
        }
        rx
    }

    /// Snapshots the visible viewport at the display's backing scale.
    pub fn screenshot(&self) -> oneshot::Receiver<Result<Screenshot, String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Cell::new(Some(tx));
        let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            // SAFETY: valid-or-nil pointers for the callback's duration.
            let outcome = unsafe {
                match (image.as_ref(), error.as_ref()) {
                    (_, Some(error)) => Err(error.localizedDescription().to_string()),
                    (Some(image), None) => png_from_image(image),
                    (None, None) => Err("WebKit returned no image".to_owned()),
                }
            };
            if let Some(tx) = tx.take() {
                let _ = tx.send(outcome);
            }
        });
        let mtm = self.wk.mtm();
        let width = self.wk.bounds().size.width;
        // SAFETY: main thread; the configuration and block outlive the call
        // (WebKit retains them).
        unsafe {
            let config = WKSnapshotConfiguration::new(mtm);
            config.setAfterScreenUpdates(true);
            if width > 0.0 {
                config.setSnapshotWidth(Some(&NSNumber::new_f64(width)));
            }
            self.wk
                .takeSnapshotWithConfiguration_completionHandler(Some(&config), &handler);
        }
        rx
    }
}

impl Drop for NativeWebView {
    fn drop(&mut self) {
        // wry removes the view from its superview; make sure GPUI keeps key
        // focus rather than the window losing its first responder.
        self.release_focus();
    }
}

/// Converts a `callAsyncJavaScript` completion into our result type.
///
/// # Safety
/// The references must be the live objects WebKit passed to the callback.
unsafe fn js_outcome(
    result: Option<&AnyObject>,
    error: Option<&NSError>,
) -> Result<Option<String>, String> {
    if let Some(error) = error {
        let info = error.userInfo();
        let message = info
            .objectForKey(ns_string!("WKJavaScriptExceptionMessage"))
            .and_then(|m| m.downcast::<NSString>().ok())
            .map(|m| m.to_string())
            .unwrap_or_else(|| error.localizedDescription().to_string());
        return Err(message);
    }
    match result {
        None => Ok(None),
        Some(obj) => match obj.downcast_ref::<NSString>() {
            Some(s) => Ok(Some(s.to_string())),
            None => Err(format!(
                "the script returned a non-string value ({})",
                obj.class().name().to_string_lossy()
            )),
        },
    }
}

pub(super) fn png_from_image(image: &NSImage) -> Result<Screenshot, String> {
    let tiff = image
        .TIFFRepresentation()
        .ok_or("could not read the snapshot bitmap")?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("could not decode the snapshot")?;
    // SAFETY: an empty properties dictionary is valid for PNG encoding.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("PNG encoding failed")?;
    let width = rep.pixelsWide().max(0) as u32;
    let points = image.size().width;
    Ok(Screenshot {
        png: png.to_vec(),
        width,
        height: rep.pixelsHigh().max(0) as u32,
        scale: if points > 0.0 {
            f64::from(width) / points
        } else {
            1.0
        },
    })
}
