//! [`BrowserView`]: toolbar, native web view, and the automation entry point.

use crate::address::{DEFAULT_SEARCH_TEMPLATE, SchemePolicy, normalize_address, scheme_policy};
use crate::command::{
    BrowserCommand, BrowserError, EVAL_TIMEOUT, NAVIGATION_TIMEOUT, SCRIPT_TIMEOUT,
};
use crate::menu::{self, Entry};
use crate::native::{JsWorld, LoadMark, NativeEvent, NativeWebView, PageState};
use crate::script::{self, Helper};
use crate::snapshot::{MAX_SNAPSHOT_CHARS, PageSnapshot, format_snapshot};
use crate::surface::Surface;
use base64::Engine as _;
use futures::StreamExt as _;
use futures::future::{Either, select};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, BackgroundExecutor, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, actions, canvas, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::{Bounds, Pixels};
use serde_json::{Value, json};
use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

actions!(
    loft_browser,
    [GoBack, GoForward, ReloadPage, StopLoading, FocusAddressBar]
);

const KEY_CONTEXT: &str = "BrowserView";
const TOOLBAR_HEIGHT: f32 = 40.;
/// How often the view re-reads URL/title/progress while a page loads, and
/// while idle (catches `pushState` URL changes and focus moves).
const BUSY_POLL: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(400);
/// Cap on the JSON text `Eval` returns.
const MAX_EVAL_CHARS: usize = 100_000;

/// Binds the browser's shortcuts (active while the view has focus).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-l", FocusAddressBar, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-r", ReloadPage, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-[", GoBack, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-]", GoForward, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-.", StopLoading, Some(KEY_CONTEXT)),
    ]);
}

/// Emitted as the page changes, whoever caused it (user, page or agent).
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserEvent {
    UrlChanged(String),
    TitleChanged(String),
    LoadingChanged(bool),
    /// The page started or stopped making sound, or was muted or unmuted.
    AudioChanged,
    /// The user left a note on an element in annotation mode.
    Annotation(Annotation),
    /// The user took a screenshot of the page (PNG).
    Screenshot(Vec<u8>),
    /// Annotation mode was turned off.
    AnnotatingStopped,
}

/// A note the user pinned to an element of the page.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Annotation {
    pub selector: String,
    pub note: String,
    /// The element's visible text (trimmed), to recognise it by.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tag: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// The pin number shown on the page.
    #[serde(default)]
    pub n: u32,
    /// For a dragged area: where it is on the page (CSS px).
    #[serde(default)]
    pub rect: Option<AreaRect>,
}

impl Annotation {
    /// A dragged-out area rather than one element.
    pub fn is_area(&self) -> bool {
        self.rect.is_some()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AreaRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A screen to emulate: the page lays out at this size (scaled to fit).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Device {
    pub name: &'static str,
    pub width: f32,
    pub height: f32,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Desktop,
    Tablet,
    Phone,
}

/// Current screens (CSS viewport sizes), checked November 2026.
pub const DEVICES: &[Device] = &[
    Device {
        name: "Desktop",
        width: 1440.,
        height: 900.,
        kind: DeviceKind::Desktop,
    },
    Device {
        name: "Laptop",
        width: 1280.,
        height: 800.,
        kind: DeviceKind::Desktop,
    },
    Device {
        name: "iPad Pro 13″",
        width: 1032.,
        height: 1376.,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad Pro 11″",
        width: 834.,
        height: 1210.,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad Air",
        width: 820.,
        height: 1180.,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPad mini",
        width: 744.,
        height: 1133.,
        kind: DeviceKind::Tablet,
    },
    Device {
        name: "iPhone 18 Pro Max",
        width: 440.,
        height: 956.,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone 18 Pro",
        width: 402.,
        height: 874.,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone Air",
        width: 420.,
        height: 912.,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "iPhone 17e",
        width: 390.,
        height: 844.,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "Pixel 10 Pro",
        width: 410.,
        height: 914.,
        kind: DeviceKind::Phone,
    },
    Device {
        name: "Android compact",
        width: 360.,
        height: 800.,
        kind: DeviceKind::Phone,
    },
];

// Safari on iOS 26 keeps reporting 18_6 as the OS; Chrome reduces Android's.
const UA_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const UA_IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 18_6 like Mac OS X) AppleWebKit/605.1.15 \
    (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const UA_ANDROID: &str = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/142.0.0.0 Mobile Safari/537.36";

/// The page width the mini preview lays out at.
const PREVIEW_PAGE_WIDTH: f32 = 1280.;

const ANNOTATE_JS: &str = include_str!("js/annotate.js");

/// An embedded browser: a gpui-kit toolbar over a native web view that an
/// agent can drive with [`BrowserView::execute`].
pub struct BrowserView {
    focus_handle: FocusHandle,
    native: Option<Rc<NativeWebView>>,
    /// Why there is no native view (unsupported platform, creation error).
    unavailable: Option<SharedString>,
    address: Entity<InputState>,
    /// The user is typing in the address bar; don't overwrite it.
    editing_address: bool,
    /// Text to put in the address bar on the next render.
    pending_address: Option<String>,
    page: PageState,
    visible: bool,
    /// Error banner under the toolbar (bad address, failed or blocked load).
    notice: Option<SharedString>,
    /// The last host-initiated load, to explain it if it fails.
    host_load: Option<(LoadMark, String)>,
    search_template: String,
    /// The emulated screen (`None` = fill the panel).
    device: Option<usize>,
    rotated: bool,
    /// The content area's size at the last paint, to fit a device in.
    area: Rc<Cell<Option<(f32, f32)>>>,
    /// Annotation mode: clicks leave notes instead of acting on the page.
    annotating: bool,
    /// The screen-size menu is open (the native page is hidden meanwhile).
    /// Where the device and ⋮ buttons are (menus open under them).
    device_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    more_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The user's page zoom (1.0 = 100%), on top of any device scaling.
    user_zoom: f32,
    /// While a dialog is open the native page (which draws above
    /// everything) gives way to a still picture of itself, so the dialog's
    /// dimmed backdrop covers it like the rest of the window.
    freeze: Freeze,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// See [`BrowserView::freeze`].
enum Freeze {
    Live,
    /// A dialog opened; the picture is on its way (the page stays live).
    Pending,
    Still(std::sync::Arc<gpui_kit::Image>),
    /// No picture: hide the page.
    Hidden,
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let address =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search or enter address"));
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let (native, unavailable) = match NativeWebView::new(window, events_tx) {
            Ok(native) => (Some(Rc::new(native)), None),
            Err(err) => {
                tracing::warn!("loft-browser: no native web view: {err:#}");
                (None, Some(SharedString::from(format!("{err:#}"))))
            }
        };

        let subscriptions = vec![
            cx.subscribe_in(&address, window, Self::on_address_event),
            // GPUI focus left the page (a shortcut, another panel): hand
            // AppKit key focus back to GPUI as well.
            cx.on_blur(&focus_handle, window, |this, _, _| {
                if let Some(native) = &this.native {
                    native.release_focus();
                }
            }),
        ];

        let mut tasks = Vec::new();
        if native.is_some() {
            // WebKit delegate callbacks only enqueue events; apply them here,
            // outside any GPUI update.
            tasks.push(cx.spawn_in(window, async move |this, cx| {
                while let Some(event) = events_rx.next().await {
                    if this
                        .update_in(cx, |this, window, cx| {
                            this.on_native_event(event, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
            tasks.push(cx.spawn_in(window, async move |this, cx| {
                loop {
                    let Ok(delay) = this.update_in(cx, |this, window, cx| this.sync(window, cx))
                    else {
                        break;
                    };
                    cx.background_executor().timer(delay).await;
                }
            }));
        }

        Self {
            focus_handle,
            native,
            unavailable,
            address,
            editing_address: false,
            pending_address: None,
            page: PageState::default(),
            visible: true,
            notice: None,
            host_load: None,
            search_template: DEFAULT_SEARCH_TEMPLATE.to_owned(),
            device: None,
            rotated: false,
            area: Rc::new(Cell::new(None)),
            annotating: false,
            device_anchor: Rc::default(),
            more_anchor: Rc::default(),
            user_zoom: 1.0,
            freeze: Freeze::Live,
            _tasks: tasks,
            _subscriptions: subscriptions,
        }
    }

    /// Navigates to a URL or address-bar style input (bare words search).
    /// Invalid or blocked input shows a notice instead.
    pub fn navigate(&mut self, url: &str, cx: &mut Context<Self>) {
        let result = normalize_address(url, &self.search_template)
            .and_then(|url| self.load_url(url.as_str(), cx));
        if let Err(err) = result {
            self.notice = Some(err.to_string().into());
            cx.notify();
        }
    }

    /// The current page URL, once a page has loaded.
    pub fn url(&self) -> Option<&str> {
        self.page.url.as_deref()
    }

    /// The current page title, if it has one.
    pub fn title(&self) -> Option<&str> {
        self.page.title.as_deref().filter(|t| !t.is_empty())
    }

    pub fn is_loading(&self) -> bool {
        self.page.loading
    }

    /// Whether the page is making sound.
    pub fn is_playing_audio(&self) -> bool {
        self.page.playing_audio
    }

    /// Whether the page's sound is muted.
    pub fn is_muted(&self) -> bool {
        self.page.muted
    }

    /// Mute or unmute the page's sound.
    pub fn set_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        let Some(native) = self.native.clone() else {
            return;
        };
        native.set_muted(muted);
        let page = native.state();
        if page != self.page {
            let audio =
                (page.playing_audio, page.muted) != (self.page.playing_audio, self.page.muted);
            self.page = page;
            if audio {
                cx.emit(BrowserEvent::AudioChanged);
            }
            cx.notify();
        }
    }

    /// Shows or hides the web view. A hidden view keeps its page and can
    /// still be driven by [`Self::execute`]. Independently of this flag the
    /// native view is hidden whenever the element is not painted.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            if !visible && let Some(native) = &self.native {
                native.set_hidden(true);
            }
            cx.notify();
        }
    }

    /// Emulate `device` (an index into [`DEVICES`]; `None` = fill the
    /// panel): the page lays out at its size and gets its user agent.
    pub fn set_device(&mut self, device: Option<usize>, cx: &mut Context<Self>) {
        if self.device == device {
            return;
        }
        let before = self.device.map(|i| DEVICES[i].kind);
        self.device = device;
        self.rotated = false;
        let kind = device.map(|i| DEVICES[i]);
        if let Some(native) = &self.native {
            let agent = match kind {
                Some(Device { name, .. })
                    if name.starts_with("Pixel") || name.starts_with("Android") =>
                {
                    Some(UA_ANDROID)
                }
                Some(Device {
                    kind: DeviceKind::Phone,
                    ..
                }) => Some(UA_IPHONE),
                Some(Device {
                    kind: DeviceKind::Tablet,
                    ..
                }) => Some(UA_IPAD),
                _ => None,
            };
            native.set_user_agent(agent);
            // Sites pick layouts from the user agent: reload when it changes.
            if before != kind.map(|d| d.kind) && self.page.url.is_some() {
                native.reload();
            }
        }
        cx.notify();
    }

    pub fn rotate(&mut self, cx: &mut Context<Self>) {
        self.rotated = !self.rotated;
        cx.notify();
    }

    /// Turn annotation mode on or off.
    pub fn set_annotating(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.annotating == on {
            return;
        }
        self.annotating = on;
        if !on {
            if let Some(native) = &self.native {
                // Fire and forget: nothing to wait for.
                drop(native.call_js(
                    "if (window.__loftAnnotate) window.__loftAnnotate.stop();",
                    JsWorld::Agent,
                ));
            }
            cx.emit(BrowserEvent::AnnotatingStopped);
        }
        cx.notify();
    }

    fn page_shown(&self) -> bool {
        self.visible
    }

    /// The page, at `zoom` times the user's zoom.
    fn page_zoomed(&self, native: Rc<NativeWebView>, zoom: f32) -> AnyElement {
        let page = match &self.freeze {
            Freeze::Live | Freeze::Pending => Surface::new(native, self.page_shown())
                .zoom(f64::from(zoom * self.user_zoom))
                .into_any_element(),
            Freeze::Still(picture) => {
                use gpui_kit::StyledImage as _;
                gpui_kit::img(picture.clone())
                    .size_full()
                    .object_fit(gpui_kit::ObjectFit::Fill)
                    .into_any_element()
            }
            Freeze::Hidden => div().size_full().into_any_element(),
        };
        div()
            .flex_1()
            .min_h_0()
            .size_full()
            .flex()
            .flex_col()
            .child(page)
            .into_any_element()
    }

    /// Swap the live page for a picture while a dialog is open (see
    /// [`Self::freeze`]), and back once it closes.
    fn follow_dialogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        let covered = window.has_active_dialog(cx);
        match (&self.freeze, covered) {
            (Freeze::Live, true) if self.visible => {
                let Some(native) = &self.native else {
                    return;
                };
                self.freeze = Freeze::Pending;
                let rx = native.screenshot();
                cx.spawn(async move |this, cx| {
                    let shot = rx.await.ok().and_then(Result::ok);
                    let _ = this.update(cx, |this, cx| {
                        if matches!(this.freeze, Freeze::Pending) {
                            this.freeze = match shot {
                                Some(shot) => {
                                    Freeze::Still(std::sync::Arc::new(gpui_kit::Image::from_bytes(
                                        gpui_kit::ImageFormat::Png,
                                        shot.png,
                                    )))
                                }
                                None => Freeze::Hidden,
                            };
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            (Freeze::Live, _) => {}
            (_, false) => {
                // The picture is done with. GPUI keeps what it decoded and
                // the texture it drew until told to let go: a full-size
                // screenshot for every dialog opened over a page (the
                // command palette is one), for as long as the app ran.
                if let Freeze::Still(picture) = std::mem::replace(&mut self.freeze, Freeze::Live) {
                    if let Some(drawn) = picture.clone().get_render_image(window, cx) {
                        cx.drop_image(drawn, Some(window));
                    }
                    picture.remove_asset(cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// A PNG of the page as shown (pins included), for attaching.
    pub fn capture(&self, cx: &mut Context<Self>) -> Option<gpui_kit::Task<Option<Vec<u8>>>> {
        let rx = self.native.as_ref()?.screenshot();
        Some(cx.background_spawn(async move { rx.await.ok()?.ok().map(|s| s.png) }))
    }

    /// Screenshot the page for the composer.
    fn take_screenshot(&mut self, cx: &mut Context<Self>) {
        let Some(task) = self.capture(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Some(png) = task.await {
                let _ = this.update(cx, |_, cx| cx.emit(BrowserEvent::Screenshot(png)));
            }
        })
        .detach();
    }

    /// Step the page zoom: `1` in, `-1` out, `0` back to 100%.
    pub fn zoom_step(&mut self, dir: i32, cx: &mut Context<Self>) {
        const STEPS: [f32; 11] = [0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0];
        let at = STEPS
            .iter()
            .position(|z| (z - self.user_zoom).abs() < 0.01)
            .unwrap_or(5);
        self.user_zoom = match dir {
            0 => 1.0,
            d if d > 0 => STEPS[(at + 1).min(STEPS.len() - 1)],
            _ => STEPS[at.saturating_sub(1)],
        };
        cx.notify();
    }

    /// WebKit's inspector, in its own window: it docks into ours a moment
    /// after opening, so keep moving it out for a short while.
    fn open_inspector(&mut self, cx: &mut Context<Self>) {
        let Some(native) = self.native.clone() else {
            return;
        };
        native.open_devtools();
        cx.spawn(async move |this, cx| {
            for _ in 0..30 {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if native.detach_devtools() {
                    // Put the page back where it belongs.
                    let _ = this.update(cx, |_, cx| cx.notify());
                    return;
                }
            }
        })
        .detach();
    }

    /// The ⋮ menu: reloads, inspector, zoom, site data.
    fn more_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.weak_entity();
        let zoom = self.user_zoom;
        let anchor = self.more_anchor.clone();
        let button = Button::new("loft-browser-more")
            .cursor_pointer()
            .small()
            .ghost()
            .icon(IconName::EllipsisVertical)
            .tooltip("More")
            .disabled(self.native.is_none())
            .on_click(move |_, window, cx| {
                let act =
                    |label: String,
                     icon: IconName,
                     f: fn(&mut BrowserView, &mut Context<BrowserView>)| {
                        let v = view.clone();
                        Entry::item(label, icon, move |cx| {
                            _ = v.update(cx, f);
                        })
                    };
                let inspecting = view
                    .upgrade()
                    .and_then(|v| v.read(cx).native.as_ref().map(|n| n.devtools_open()))
                    .unwrap_or(false);
                let entries = vec![
                    act("Hard Reload".into(), IconName::RotateCw, |this, cx| {
                        if let Some(native) = &this.native {
                            native.hard_reload();
                            cx.notify();
                        }
                    }),
                    if inspecting {
                        act("Close Web Inspector".into(), IconName::Bug, |this, _| {
                            if let Some(native) = &this.native {
                                native.close_devtools();
                            }
                        })
                    } else {
                        act("Open Web Inspector".into(), IconName::Bug, |this, cx| {
                            this.open_inspector(cx)
                        })
                    },
                    Entry::Separator,
                    act("Zoom In".into(), IconName::ZoomIn, |this, cx| {
                        this.zoom_step(1, cx)
                    }),
                    act("Zoom Out".into(), IconName::ZoomOut, |this, cx| {
                        this.zoom_step(-1, cx)
                    }),
                    act(
                        format!("Actual Size · {:.0}%", zoom * 100.),
                        IconName::Expand,
                        |this, cx| this.zoom_step(0, cx),
                    )
                    .disabled((zoom - 1.0).abs() < 0.01),
                    Entry::Separator,
                    act(
                        "Clear Cookies & Site Data".into(),
                        IconName::Cookie,
                        |this, cx| {
                            if let Some(native) = &this.native {
                                native.clear_data(crate::native::SiteData::Cookies);
                                native.reload();
                                cx.notify();
                            }
                        },
                    ),
                    act("Clear Cache".into(), IconName::Eraser, |this, _| {
                        if let Some(native) = &this.native {
                            native.clear_data(crate::native::SiteData::Cache);
                        }
                    }),
                ];
                if let Some(at) = anchor.get() {
                    menu::open(window, at, entries, cx);
                }
            });
        anchored(button, self.more_anchor.clone())
    }

    /// Remove note `n`'s pin and outline from the page.
    pub fn remove_annotation(&mut self, n: u32) {
        if let Some(native) = &self.native {
            drop(native.call_js(
                &format!(
                    "if (window.__loftAnnotate && window.__loftAnnotate.remove) window.__loftAnnotate.remove({n});"
                ),
                JsWorld::Agent,
            ));
        }
    }

    /// Remove the numbered pins from the page (after the notes were sent).
    pub fn clear_annotations(&mut self) {
        if let Some(native) = &self.native {
            drop(native.call_js(
                "if (window.__loftAnnotate && window.__loftAnnotate.clear) window.__loftAnnotate.clear();",
                JsWorld::Agent,
            ));
        }
    }

    /// The live page, small, for a floating preview `width` points wide:
    /// it lays out as a desktop page and is scaled down to fit.
    pub fn preview_surface(&self, width: f32) -> Option<AnyElement> {
        let native = self.native.clone()?;
        Some(
            Surface::new(native, self.visible)
                .zoom(f64::from(width / PREVIEW_PAGE_WIDTH))
                .into_any_element(),
        )
    }

    /// Collect notes left in annotation mode (and keep the mode installed
    /// across page loads).
    fn poll_annotations(&mut self, native: &NativeWebView, cx: &mut Context<Self>) {
        let body = format!(
            "{ANNOTATE_JS}\nwindow.__loftAnnotate.start();\n\
             return JSON.stringify(window.__loftAnnotate.take());"
        );
        let rx = native.call_js(&body, JsWorld::Agent);
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(text))) = rx.await else {
                return;
            };
            let notes: Vec<Annotation> = serde_json::from_str(&text).unwrap_or_default();
            if notes.is_empty() {
                return;
            }
            let _ = this.update(cx, |_, cx| {
                for note in notes {
                    cx.emit(BrowserEvent::Annotation(note));
                }
            });
        })
        .detach();
    }

    /// The search URL template for bare words; `{}` is the encoded query.
    pub fn set_search_template(&mut self, template: impl Into<String>) {
        self.search_template = template.into();
    }

    /// Focuses and selects the address bar.
    pub fn focus_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(native) = &self.native {
            native.release_focus();
        }
        self.address.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Runs one automation command. The task resolves with a JSON result
    /// (see the crate docs for each command's shape) or an error that
    /// downcasts to [`BrowserError`].
    pub fn execute(
        &mut self,
        cmd: BrowserCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Value>> {
        let Some(native) = self.native.clone() else {
            let reason = self
                .unavailable
                .as_ref()
                .map_or("no web view".into(), |s| s.to_string());
            return Task::ready(Err(BrowserError::Unsupported(reason).into()));
        };
        let executor = cx.background_executor().clone();
        tracing::debug!(action = cmd.action(), "loft-browser: execute");

        let spawn = |cx: &mut Context<Self>, fut: std::pin::Pin<Box<dyn Future<Output = Result<Value, BrowserError>>>>| {
            cx.spawn(async move |_, _| fut.await.map_err(anyhow::Error::from))
        };

        match cmd {
            BrowserCommand::Navigate { url } => {
                let target = match normalize_address(&url, &self.search_template) {
                    Ok(url) => url.to_string(),
                    Err(err) => return Task::ready(Err(err.into())),
                };
                let mark = match self.load_url(&target, cx) {
                    Ok(mark) => mark,
                    Err(err) => return Task::ready(Err(err.into())),
                };
                spawn(
                    cx,
                    Box::pin(async move {
                        wait_for_load(&native, &executor, mark, &target).await?;
                        Ok(page_summary(&native))
                    }),
                )
            }
            BrowserCommand::Back | BrowserCommand::Forward | BrowserCommand::Reload => {
                let state = native.state();
                let mark = native.load_mark();
                let what = match cmd {
                    BrowserCommand::Back if !state.can_go_back => {
                        return Task::ready(Err(BrowserError::Action(
                            "there is no previous page in the history".into(),
                        )
                        .into()));
                    }
                    BrowserCommand::Forward if !state.can_go_forward => {
                        return Task::ready(Err(BrowserError::Action(
                            "there is no next page in the history".into(),
                        )
                        .into()));
                    }
                    BrowserCommand::Back => {
                        native.go_back();
                        "the previous page".to_owned()
                    }
                    BrowserCommand::Forward => {
                        native.go_forward();
                        "the next page".to_owned()
                    }
                    _ => {
                        native.reload();
                        state.url.unwrap_or_else(|| "the page".into())
                    }
                };
                self.notice = None;
                cx.notify();
                spawn(
                    cx,
                    Box::pin(async move {
                        wait_for_load(&native, &executor, mark, &what).await?;
                        Ok(page_summary(&native))
                    }),
                )
            }
            BrowserCommand::Snapshot => spawn(
                cx,
                Box::pin(async move {
                    let raw = run_helper(&native, &executor, Helper::Snapshot, json!({})).await?;
                    let snapshot: PageSnapshot = serde_json::from_value(raw)
                        .map_err(|e| BrowserError::Script(format!("unreadable snapshot: {e}")))?;
                    let formatted = format_snapshot(&snapshot, MAX_SNAPSHOT_CHARS);
                    Ok(json!({
                        "url": snapshot.url,
                        "title": snapshot.title,
                        "snapshot": formatted.text,
                        "refs": formatted.refs_total,
                        "truncated": formatted.truncated,
                    }))
                }),
            ),
            BrowserCommand::Click { ref_id } => spawn(
                cx,
                Box::pin(interact(
                    native,
                    executor,
                    Helper::Click,
                    json!({ "ref": ref_id }),
                    Some(ref_id),
                )),
            ),
            BrowserCommand::Type {
                ref_id,
                text,
                submit,
            } => spawn(
                cx,
                Box::pin(interact(
                    native,
                    executor,
                    Helper::Type,
                    json!({ "ref": ref_id, "text": text, "submit": submit }),
                    Some(ref_id),
                )),
            ),
            BrowserCommand::Press { key } => spawn(
                cx,
                Box::pin(interact(
                    native,
                    executor,
                    Helper::Press,
                    json!({ "key": key }),
                    None,
                )),
            ),
            BrowserCommand::Scroll { dy } => spawn(
                cx,
                Box::pin(async move {
                    let raw =
                        run_helper(&native, &executor, Helper::Scroll, json!({ "dy": dy })).await?;
                    action_result(raw, None)
                }),
            ),
            BrowserCommand::Eval { script } => spawn(
                cx,
                Box::pin(async move { eval(&native, &executor, &script).await }),
            ),
            BrowserCommand::Screenshot => spawn(
                cx,
                Box::pin(async move {
                    let shot = with_timeout(
                        native.screenshot(),
                        SCRIPT_TIMEOUT,
                        &executor,
                        "taking a screenshot",
                    )
                    .await?
                    .map_err(|_| BrowserError::Closed)?
                    .map_err(|e| BrowserError::Action(format!("screenshot failed: {e}")))?;
                    Ok(json!({
                        "mime_type": "image/png",
                        "width": shot.width,
                        "height": shot.height,
                        "scale": shot.scale,
                        "data": base64::engine::general_purpose::STANDARD.encode(&shot.png),
                    }))
                }),
            ),
        }
    }

    // -- internals --------------------------------------------------------------

    fn load_url(&mut self, url: &str, cx: &mut Context<Self>) -> Result<LoadMark, BrowserError> {
        let Some(native) = &self.native else {
            return Err(BrowserError::Unsupported(
                self.unavailable
                    .as_ref()
                    .map_or("no web view".into(), |s| s.to_string()),
            ));
        };
        let mark = native.load_mark();
        native
            .load(url)
            .map_err(|e| BrowserError::NavigationFailed(format!("{e:#}")))?;
        self.notice = None;
        self.host_load = Some((mark, url.to_owned()));
        if !self.editing_address {
            self.pending_address = Some(url.to_owned());
        }
        cx.notify();
        Ok(mark)
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(native) = &self.native
            && self.page.can_go_back
        {
            native.go_back();
            self.notice = None;
            cx.notify();
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(native) = &self.native
            && self.page.can_go_forward
        {
            native.go_forward();
            self.notice = None;
            cx.notify();
        }
    }

    /// Reload the page.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(native) = &self.native {
            native.reload();
            cx.notify();
        }
    }

    fn reload_or_stop(&mut self, cx: &mut Context<Self>) {
        if let Some(native) = &self.native {
            if self.page.loading {
                native.stop();
            } else {
                native.reload();
                self.notice = None;
            }
            cx.notify();
        }
    }

    fn on_native_event(&mut self, event: NativeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            NativeEvent::Load | NativeEvent::TitleChanged | NativeEvent::Blocked(_) => {
                self.sync(window, cx);
            }
            NativeEvent::OpenInPlace(url) => {
                // Pages may open http(s)/about targets; never file:.
                if scheme_policy(&url) == SchemePolicy::Allowed
                    && let Err(err) = self.load_url(&url, cx)
                {
                    tracing::warn!("loft-browser: could not open {url}: {err}");
                }
            }
        }
    }

    /// Re-reads the native state, emits events for what changed, and returns
    /// how long to wait before the next poll.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Duration {
        let Some(native) = self.native.clone() else {
            return IDLE_POLL;
        };
        let page = native.state();
        if page != self.page {
            if page.url != self.page.url {
                if let Some(url) = &page.url {
                    cx.emit(BrowserEvent::UrlChanged(url.clone()));
                }
                if !self.editing_address {
                    self.pending_address = Some(page.url.clone().unwrap_or_default());
                }
            }
            if page.title != self.page.title {
                cx.emit(BrowserEvent::TitleChanged(
                    page.title.clone().unwrap_or_default(),
                ));
            }
            if page.loading != self.page.loading {
                cx.emit(BrowserEvent::LoadingChanged(page.loading));
            }
            if (page.playing_audio, page.muted) != (self.page.playing_audio, self.page.muted) {
                cx.emit(BrowserEvent::AudioChanged);
            }
            self.page = page;
            cx.notify();
        }
        if !self.page.loading
            && let Some((mark, url)) = self.host_load.take()
        {
            let progress = native.load_progress(mark);
            // A finished load succeeded even if a subframe was blocked.
            if !progress.finished && (progress.started || progress.blocked.is_some()) {
                self.notice = Some(match progress.blocked {
                    Some(blocked) => format!(
                        "Blocked {blocked}: only http, https and about URLs (and files the app opens) may load"
                    ),
                    None => format!("Couldn't load {url}"),
                }
                .into());
                cx.notify();
            } else if !progress.started && !progress.finished {
                // Not begun yet; check again next time.
                self.host_load = Some((mark, url));
            }
        }
        if self.annotating && !self.page.loading {
            self.poll_annotations(&native, cx);
        }
        // Clicking into the page gives it AppKit key focus; mirror that in
        // GPUI so the address bar blurs and the browser's shortcuts apply.
        if native.is_focused() && !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        if self.page.loading {
            BUSY_POLL
        } else {
            IDLE_POLL
        }
    }

    fn on_address_event(
        &mut self,
        state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                let input = state.read(cx).value().to_string();
                self.editing_address = false;
                self.navigate(&input, cx);
                window.focus(&self.focus_handle, cx);
            }
            InputEvent::Focus => {
                self.editing_address = true;
                state.update(cx, |state, cx| state.select_all(window, cx));
            }
            InputEvent::Blur => {
                self.editing_address = false;
                self.pending_address = Some(self.page.url.clone().unwrap_or_default());
                cx.notify();
            }
            InputEvent::Change => {}
        }
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_dialogs(window, cx);
        if !self.editing_address
            && let Some(text) = self.pending_address.take()
        {
            let display = if text == "about:blank" {
                String::new()
            } else {
                text
            };
            self.address
                .update(cx, |state, cx| state.set_value(display, window, cx));
        }

        let theme = cx.theme();
        let (background, border, muted_foreground, primary, danger) = (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.primary,
            theme.danger,
        );
        let loading = self.page.loading;
        let progress = self.page.progress.clamp(0.08, 1.0) as f32;

        let address_prefix = if loading {
            Spinner::new()
                .xsmall()
                .color(muted_foreground)
                .into_any_element()
        } else {
            let icon = match self.page.url.as_deref() {
                Some(url) if url.starts_with("file:") => IconName::File,
                _ => IconName::Globe,
            };
            Icon::new(icon)
                .xsmall()
                .text_color(muted_foreground)
                .into_any_element()
        };

        let toolbar = h_flex()
            .relative()
            .h(px(TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("loft-browser-back")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::ArrowLeft)
                    .accessibility_label("Back")
                    .disabled(!self.page.can_go_back)
                    .on_click(cx.listener(|this, _, _, cx| this.go_back(cx))),
            )
            .child(
                Button::new("loft-browser-forward")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::ArrowRight)
                    .accessibility_label("Forward")
                    .disabled(!self.page.can_go_forward)
                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
            )
            .child(
                Button::new("loft-browser-reload")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(if loading {
                        IconName::Close
                    } else {
                        IconName::RotateCw
                    })
                    .accessibility_label(if loading { "Stop" } else { "Reload" })
                    .disabled(self.native.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.reload_or_stop(cx))),
            )
            .child(
                div().flex_1().min_w_0().pl_1().child(
                    Input::new(&self.address)
                        .small()
                        .prefix(address_prefix)
                        .disabled(self.native.is_none()),
                ),
            )
            .child(
                Button::new("loft-browser-shot")
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::Camera)
                    .tooltip("Screenshot to the composer")
                    .disabled(self.native.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.take_screenshot(cx))),
            )
            .child(self.device_menu(cx))
            .child(
                Button::new("loft-browser-annotate")
                    .cursor_pointer()
                    .small()
                    .icon(IconName::MessageSquarePlus)
                    .when(self.annotating, |b| b.primary())
                    .when(!self.annotating, |b| b.ghost())
                    .tooltip(if self.annotating {
                        "Stop annotating"
                    } else {
                        "Annotate: click an element or drag over an area to leave notes for the agent"
                    })
                    .disabled(self.native.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let on = !this.annotating;
                        this.set_annotating(on, cx);
                    })),
            )
            .child(self.more_menu(cx))
            .when(loading, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .bottom(px(-1.))
                        .h(px(2.))
                        .w(relative(progress))
                        .bg(primary),
                )
            });

        let notice = self.notice.clone().map(|notice| {
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(border)
                .text_xs()
                .text_color(danger)
                .child(Icon::new(IconName::TriangleAlert).xsmall())
                .child(div().flex_1().min_w_0().truncate().child(notice))
                .child(
                    Button::new("loft-browser-dismiss")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .accessibility_label("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                )
        });

        let content = match &self.native {
            Some(native) => match self.device {
                None => div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.page_zoomed(native.clone(), 1.))
                    .into_any_element(),
                Some(ix) => self.device_frame(native.clone(), DEVICES[ix], cx),
            },
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .text_sm()
                .text_color(muted_foreground)
                .child(Icon::new(IconName::Globe).large())
                .child(self.unavailable.clone().unwrap_or_default())
                .into_any_element(),
        };

        let annotate_bar = self.annotating.then(|| {
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(border)
                .bg(primary.opacity(0.12))
                .text_xs()
                .child(
                    Icon::new(IconName::MousePointerClick)
                        .xsmall()
                        .text_color(primary),
                )
                .child(div().flex_1().min_w_0().child(
                    "Click an element or drag over an area to leave a note. Notes wait in the composer until you send.",
                ))
                .child(
                    Button::new("loft-browser-annotate-done")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .label("Done")
                        .on_click(cx.listener(|this, _, _, cx| this.set_annotating(false, cx))),
                )
        });

        v_flex()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &ReloadPage, _, cx| {
                if let Some(native) = &this.native {
                    native.reload();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &StopLoading, _, cx| {
                if let Some(native) = &this.native {
                    native.stop();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &FocusAddressBar, window, cx| {
                this.focus_address_bar(window, cx)
            }))
            .child(toolbar)
            .children(notice)
            .children(annotate_bar)
            .child(content)
    }
}

impl BrowserView {
    /// The toolbar button: shows the current screen and opens a menu of
    /// screens.
    fn device_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = match self.device.map(|i| DEVICES[i]) {
            None => (IconName::Expand, "Responsive".to_owned()),
            Some(d) => (device_icon(d.kind), d.name.to_owned()),
        };
        let (device, rotated) = (self.device, self.rotated);
        let view = cx.weak_entity();
        let anchor = self.device_anchor.clone();
        let button = Button::new("loft-browser-device")
            .cursor_pointer()
            .small()
            .icon(icon)
            .when(device.is_some(), |b| b.primary())
            .when(device.is_none(), |b| b.ghost())
            .tooltip(format!("Screen size: {label}"))
            .disabled(self.native.is_none())
            .on_click(move |_, window, cx| {
                let pick = |name: &str, icon: IconName, ix: Option<usize>| {
                    let v = view.clone();
                    Entry::item(name.to_owned(), icon, move |cx| {
                        _ = v.update(cx, |this, cx| this.set_device(ix, cx));
                    })
                    .checked(device == ix)
                };
                let mut entries =
                    vec![pick("Responsive", IconName::Expand, None), Entry::Separator];
                let mut last = None;
                for (ix, d) in DEVICES.iter().enumerate() {
                    if last.is_some_and(|k| k != d.kind) {
                        entries.push(Entry::Separator);
                    }
                    last = Some(d.kind);
                    entries.push(pick(d.name, device_icon(d.kind), Some(ix)));
                }
                let v = view.clone();
                entries.push(Entry::Separator);
                entries.push(
                    Entry::item("Rotate", IconName::RotateCwSquare, move |cx| {
                        _ = v.update(cx, |this, cx| this.rotate(cx));
                    })
                    .checked(rotated)
                    .disabled(device.is_none()),
                );
                if let Some(at) = anchor.get() {
                    menu::open(window, at, entries, cx);
                }
            });
        anchored(button, self.device_anchor.clone())
    }

    /// The page at the device's size, scaled down to fit the panel.
    fn device_frame(
        &self,
        native: Rc<NativeWebView>,
        device: Device,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, muted_foreground) = (theme.muted, theme.border, theme.muted_foreground);
        let (w, h) = if self.rotated {
            (device.height, device.width)
        } else {
            (device.width, device.height)
        };
        let (aw, ah) = self.area.get().unwrap_or((w, h));
        let scale = ((aw - 32.) / w).min((ah - 56.) / h).clamp(0.1, 1.0);
        let area = self.area.clone();
        let measure = canvas(
            move |bounds, window, _| {
                let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                if area.get() != Some(size) {
                    area.set(Some(size));
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let radius = match device.kind {
            DeviceKind::Phone => 22.,
            DeviceKind::Tablet => 14.,
            DeviceKind::Desktop => 6.,
        };
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .bg(muted)
            .child(measure)
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(w * scale))
                            .h(px(h * scale))
                            .rounded(px(radius * scale.max(0.5)))
                            .overflow_hidden()
                            .border_1()
                            .border_color(border)
                            .shadow_lg()
                            .flex()
                            .flex_col()
                            .child(self.page_zoomed(native, scale)),
                    )
                    .child(div().text_xs().text_color(muted_foreground).child(format!(
                        "{} · {} × {} · {:.0}%",
                        device.name,
                        w as u32,
                        h as u32,
                        scale * 100.
                    ))),
            )
            .into_any_element()
    }
}

// -- automation helpers (run on the foreground executor) ----------------------------

fn page_summary(native: &NativeWebView) -> Value {
    let state = native.state();
    json!({ "url": state.url, "title": state.title.unwrap_or_default() })
}

async fn with_timeout<F: Future>(
    fut: F,
    after: Duration,
    executor: &BackgroundExecutor,
    what: &str,
) -> Result<F::Output, BrowserError> {
    let timer = executor.timer(after);
    match select(pin!(fut), pin!(timer)).await {
        Either::Left((out, _)) => Ok(out),
        Either::Right(_) => Err(BrowserError::Timeout {
            what: what.to_owned(),
            after,
        }),
    }
}

/// Runs a helper from `agent.js` in the isolated world and parses its JSON.
async fn run_helper(
    native: &NativeWebView,
    executor: &BackgroundExecutor,
    helper: Helper,
    args: Value,
) -> Result<Value, BrowserError> {
    let body = script::helper_call(helper, &args);
    let reply = with_timeout(
        native.call_js(&body, JsWorld::Agent),
        SCRIPT_TIMEOUT,
        executor,
        "waiting for the page to run the automation script",
    )
    .await?
    .map_err(|_| BrowserError::Closed)?
    .map_err(BrowserError::Script)?
    .ok_or_else(|| BrowserError::Script("the automation script returned nothing".into()))?;
    serde_json::from_str(&reply)
        .map_err(|e| BrowserError::Script(format!("unreadable automation result: {e}")))
}

/// `{ok: true, …}` → the rest of the object; `{ok: false, code, error}` → error.
fn action_result(mut value: Value, ref_id: Option<u32>) -> Result<Value, BrowserError> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(obj) = value.as_object_mut() {
            obj.remove("ok");
        }
        return Ok(value);
    }
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the action failed")
        .to_owned();
    Err(match (code, ref_id) {
        ("ref_not_found", Some(id)) => BrowserError::RefNotFound(id),
        _ => BrowserError::Action(message),
    })
}

/// Click/type/press: run the helper, then wait for any navigation it caused.
async fn interact(
    native: Rc<NativeWebView>,
    executor: BackgroundExecutor,
    helper: Helper,
    args: Value,
    ref_id: Option<u32>,
) -> Result<Value, BrowserError> {
    let mark = native.load_mark();
    let url_before = native.state().url;
    let raw = run_helper(&native, &executor, helper, args).await?;
    let mut result = action_result(raw, ref_id)?;

    // Give a navigation triggered by the action a moment to start.
    let start = Instant::now();
    let mut navigated = false;
    while start.elapsed() < Duration::from_millis(500) {
        let progress = native.load_progress(mark);
        if progress.loading || progress.started || progress.finished {
            navigated = true;
            break;
        }
        executor.timer(Duration::from_millis(50)).await;
    }
    if navigated && let Err(err) = wait_for_load(&native, &executor, mark, "the new page").await {
        result["navigation_error"] = json!(err.to_string());
    }
    let state = native.state();
    result["navigated"] = json!(navigated || state.url != url_before);
    result["url"] = json!(state.url);
    result["title"] = json!(state.title.unwrap_or_default());
    Ok(result)
}

/// Waits until a load that began after `mark` finishes.
///
/// WebKit reports start (commit) and finish, but not provisional failures,
/// so a load that stops without finishing is reported as failed. A
/// navigation that never starts (same-document, a no-op) succeeds after a
/// short grace period.
async fn wait_for_load(
    native: &NativeWebView,
    executor: &BackgroundExecutor,
    mark: LoadMark,
    what: &str,
) -> Result<(), BrowserError> {
    const POLL: Duration = Duration::from_millis(50);
    const STOP_GRACE: Duration = Duration::from_millis(300);
    const NEVER_STARTED: Duration = Duration::from_millis(1500);
    let start = Instant::now();
    let mut active = false;
    let mut stopped_since: Option<Instant> = None;
    loop {
        let progress = native.load_progress(mark);
        if progress.finished && !progress.loading {
            return Ok(());
        }
        active |= progress.loading || progress.started;
        if progress.loading {
            stopped_since = None;
        } else if active {
            if stopped_since.get_or_insert_with(Instant::now).elapsed() >= STOP_GRACE {
                return Err(match progress.blocked {
                    Some(url) => BrowserError::Blocked(format!(
                        "loading {what} was redirected to {url}, which is not an http, https or about URL"
                    )),
                    None => BrowserError::NavigationFailed(format!(
                        "{what} did not load (the host could not be reached, the connection failed, \
                         or the load was cancelled)"
                    )),
                });
            }
        } else if start.elapsed() >= NEVER_STARTED {
            return match progress.blocked {
                Some(url) => Err(BrowserError::Blocked(format!(
                    "{url} is not an http, https or about URL"
                ))),
                None => Ok(()),
            };
        }
        if start.elapsed() >= NAVIGATION_TIMEOUT {
            let state = native.state();
            return Err(BrowserError::Timeout {
                what: format!(
                    "waiting for {what} to load (still loading; currently at {})",
                    state.url.unwrap_or_else(|| "no page".into())
                ),
                after: NAVIGATION_TIMEOUT,
            });
        }
        executor.timer(POLL).await;
    }
}

/// `Eval`: an expression, or an async function body if it isn't one.
async fn eval(
    native: &NativeWebView,
    executor: &BackgroundExecutor,
    script: &str,
) -> Result<Value, BrowserError> {
    // Compiles without running, so nothing executes twice.
    let probe = with_timeout(
        native.call_js(&script::eval_probe(script), JsWorld::Page),
        SCRIPT_TIMEOUT,
        executor,
        "compiling the script",
    )
    .await?;
    let is_expression = matches!(probe, Ok(Ok(Some(_))));
    let body = if is_expression {
        script::eval_expression(script)
    } else {
        script::eval_function_body(script)
    };
    let reply = with_timeout(
        native.call_js(&body, JsWorld::Page),
        EVAL_TIMEOUT,
        executor,
        "waiting for the script to finish",
    )
    .await?
    .map_err(|_| BrowserError::Closed)?
    .map_err(BrowserError::Script)?
    .unwrap_or_else(|| "null".to_owned());
    if reply.len() > MAX_EVAL_CHARS {
        let mut cut = MAX_EVAL_CHARS;
        while !reply.is_char_boundary(cut) {
            cut -= 1;
        }
        return Ok(json!({
            "truncated": true,
            "total_chars": reply.len(),
            "json_prefix": &reply[..cut],
        }));
    }
    serde_json::from_str(&reply)
        .map_err(|e| BrowserError::Script(format!("the result is not valid JSON: {e}")))
}

/// `button`, recording where it is so a menu can open under it.
fn anchored(button: Button, at: Rc<Cell<Option<Bounds<Pixels>>>>) -> AnyElement {
    div()
        .relative()
        .child(button)
        .child(
            canvas(move |bounds, _, _| at.set(Some(bounds)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .into_any_element()
}

fn device_icon(kind: DeviceKind) -> IconName {
    match kind {
        DeviceKind::Desktop => IconName::Monitor,
        DeviceKind::Tablet => IconName::Tablet,
        DeviceKind::Phone => IconName::Smartphone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_results() {
        let ok = action_result(json!({"ok": true, "clicked": "link \"x\""}), Some(1)).unwrap();
        assert_eq!(ok, json!({"clicked": "link \"x\""}));
        assert_eq!(
            action_result(
                json!({"ok": false, "code": "ref_not_found", "error": "e"}),
                Some(9)
            ),
            Err(BrowserError::RefNotFound(9))
        );
        assert_eq!(
            action_result(
                json!({"ok": false, "code": "disabled", "error": "button is disabled"}),
                Some(2)
            ),
            Err(BrowserError::Action("button is disabled".into()))
        );
        assert!(matches!(
            action_result(json!(null), None),
            Err(BrowserError::Action(_))
        ));
    }
}
