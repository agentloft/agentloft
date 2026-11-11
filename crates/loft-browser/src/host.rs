//! A native web view for a page the app writes itself: a view an agent
//! drew in the chat, shown as the real thing rather than as pictures of it.
//!
//! A hosted page is not a browser: it has one document, at an address made
//! up for it, which the owner writes into ([`ViewHost::run`]); it loads
//! nothing but scripts, styles and fonts from a short list of hosts; it
//! cannot go anywhere else; and the only way out of it is what a bridge
//! script posts from a world of its own ([`HostEvent::Message`]).
//!
//! # Shown, or waiting
//!
//! A native view draws above everything GPUI draws, so a page is only
//! *shown* (see [`HostSurface`]) while that is safe: where the pointer is.
//! Otherwise it waits out of sight, still running, and its owner draws a
//! picture of it ([`ViewHost::snapshot`]) that menus and popups can cover
//! and that scrolls in step with everything else.
//!
//! macOS only (WKWebView). Elsewhere [`ViewHost::new`] fails and the owner
//! falls back to the daemon's streamed page.

use crate::native::NativeHost;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui_kit::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, Window, relative,
};
use std::panic::Location;
use std::rc::{Rc, Weak};

/// Something a hosted page did. Sent from WebKit callbacks, which never
/// touch GPUI themselves.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// The empty document is loaded and the network rules are in place:
    /// content can be written. Said again if the page is ever made to load
    /// anew, when it is empty again.
    Ready,
    /// What the bridge posted, as it posted it (JSON).
    Message(String),
    /// The page asked for a window of its own at this address.
    Open(String),
    /// The wheel turned over the page and was handed to the thread.
    Wheel,
}

/// Which of a shown page's corners are rounded (when that edge of it is in
/// sight at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Corners {
    pub top: bool,
    pub bottom: bool,
}

impl Corners {
    /// `CACornerMask` for a page at `y..y+h` of which `vy..vy+vh` shows: an
    /// edge that is cut off has square corners.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn mask(self, y: f64, h: f64, vy: f64, vh: f64) -> usize {
        // Min-Y is the bottom edge in AppKit's unflipped coordinates.
        const BOTTOM: usize = 0b0011;
        const TOP: usize = 0b1100;
        let mut mask = 0;
        if self.top && (vy - y).abs() < 0.5 {
            mask |= TOP;
        }
        if self.bottom && ((vy + vh) - (y + h)).abs() < 0.5 {
            mask |= BOTTOM;
        }
        mask
    }
}

/// A picture of a hosted page: its pixels as GPUI draws them (BGRA, rows
/// top to bottom), at the display's density.
#[derive(Debug, Clone)]
pub struct HostPicture {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A page the app hosts natively. Cheap to clone (one page behind it).
#[derive(Clone)]
pub struct ViewHost {
    native: Rc<NativeHost>,
}

impl ViewHost {
    /// A page at `origin` (`https://…`, made up for it), waiting out of
    /// sight until [`HostEvent::Ready`].
    pub fn new(
        window: &Window,
        origin: &str,
        allowed_hosts: &[&str],
        events: UnboundedSender<HostEvent>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            native: NativeHost::new(window, origin, allowed_hosts, events)?,
        })
    }

    /// Runs `script` (an async function body) in the page's own world.
    pub fn run(&self, script: &str) -> oneshot::Receiver<Result<(), String>> {
        self.native.run(script, false)
    }

    /// Runs `script` in the bridge's world: the only one that can post
    /// [`HostEvent::Message`]s, as `globalThis.__agentloft(json)`.
    pub fn run_bridge(&self, script: &str) -> oneshot::Receiver<Result<(), String>> {
        // What only this world can do: post to us. And no menu of the web
        // view's own (its "Reload" would empty the page).
        let post = "globalThis.__agentloft = (message) => \
                    window.webkit.messageHandlers.agentloftView.postMessage(String(message));\
                    if (!globalThis.__agentloftMenu) { globalThis.__agentloftMenu = true; \
                    document.addEventListener('contextmenu', (e) => e.preventDefault(), true); }";
        self.native.run(&format!("{post}\n{script}"), true)
    }

    /// A picture of the whole page as it is now, shown or not.
    pub fn snapshot(&self) -> impl Future<Output = Result<HostPicture, String>> + use<> {
        let taken = self.native.snapshot();
        async move { taken.await.map_err(|_| "the view went away".to_owned())? }
    }

    /// The element that shows the page where it is painted while `shown`,
    /// and keeps it the element's size either way.
    pub fn surface(&self, shown: bool, radius: Pixels, corners: Corners) -> HostSurface {
        HostSurface {
            native: self.native.clone(),
            shown,
            radius,
            corners,
        }
    }

    /// Show the page now at `bounds`, as much of it as is inside
    /// `visible` (window coordinates), without waiting for the next paint:
    /// for a press that is to reach it.
    pub fn show_now(
        &self,
        bounds: Bounds<Pixels>,
        visible: Bounds<Pixels>,
        scale: f32,
        radius: Pixels,
        corners: Corners,
    ) {
        place(&self.native, bounds, visible, scale, radius, corners);
    }

    /// Whether the page is in sight right now.
    pub fn is_shown(&self) -> bool {
        self.native.is_shown()
    }

    /// Take the page out of sight now (it keeps running).
    pub fn park(&self) {
        self.native.park();
    }

    /// Whether the wheel is the page's own once it is clicked into (it has
    /// more than it shows), rather than the thread's.
    pub fn set_scrolls(&self, scrolls: bool) {
        self.native.set_scrolls(scrolls);
    }

    /// The press GPUI is handling right now belongs to the page: it was
    /// only a picture when the press landed.
    pub fn take_press(&self) {
        self.native.take_press();
    }

    /// The release GPUI is handling right now ends a press the page was
    /// handed with [`Self::take_press`].
    pub fn take_release(&self) {
        self.native.take_release();
    }

    /// Tell the page the pointer is no longer on it (before it is taken
    /// out from under the pointer, which it wouldn't notice).
    pub fn pointer_left(&self) {
        self.native.pointer_left();
    }

    pub fn is_focused(&self) -> bool {
        self.native.is_focused()
    }

    /// Hands key focus back to the app if the page has it.
    pub fn release_focus(&self) {
        self.native.release_focus();
    }

    /// A real click at `(x, y)` from the page's top left, through the
    /// window's own event handling (dev hooks).
    pub fn click_at(&self, x: f32, y: f32) {
        self.native.click_at(f64::from(x), f64::from(y));
    }

    /// A real click at `at` in the window, wherever that lands: on the
    /// page if it is shown there, on its picture if not (dev hooks).
    pub fn click_in_window(&self, at: gpui_kit::Point<Pixels>) {
        self.native
            .click_in_window(f64::from(f32::from(at.x)), f64::from(f32::from(at.y)));
    }

    /// A turn of the wheel over `(x, y)`, `lines` down (dev hooks).
    pub fn wheel_at(&self, x: f32, y: f32, lines: i32) {
        self.native.wheel_at(f64::from(x), f64::from(y), lines);
    }
}

/// Shows a hosted page where this element is painted (while it is to be
/// shown), clipped to what of the element is in sight; takes it out of
/// sight on the first frame the element isn't painted.
pub struct HostSurface {
    native: Rc<NativeHost>,
    shown: bool,
    radius: Pixels,
    corners: Corners,
}

/// Parks the page when dropped, unless a newer paint superseded it.
struct PaintGuard {
    token: u64,
    native: Weak<NativeHost>,
}

impl PaintGuard {
    fn new(native: &Rc<NativeHost>) -> Self {
        let token = native.paint_token.get() + 1;
        native.paint_token.set(token);
        Self {
            token,
            native: Rc::downgrade(native),
        }
    }
}

impl Drop for PaintGuard {
    fn drop(&mut self) {
        if let Some(native) = self.native.upgrade()
            && native.paint_token.get() == self.token
        {
            native.park();
        }
    }
}

impl IntoElement for HostSurface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for HostSurface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("loft-view-host".into())
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        if let Some(id) = id {
            let native = &self.native;
            window.with_element_state::<PaintGuard, _>(id, |guard, _| {
                ((), guard.unwrap_or_else(|| PaintGuard::new(native)))
            });
        }
        let visible = bounds.intersect(&window.content_mask().bounds);
        if self.shown {
            place(
                &self.native,
                bounds,
                visible,
                window.scale_factor(),
                self.radius,
                self.corners,
            );
        } else {
            let whole = rect(bounds, window.scale_factor());
            self.native.set_size(whole[2], whole[3]);
            self.native.park();
        }
    }
}

/// `[x, y, w, h]` in points, on whole device pixels.
fn rect(bounds: Bounds<Pixels>, scale: f32) -> [f64; 4] {
    let scale = f64::from(scale);
    let snap = |v: Pixels| (f64::from(f32::from(v)) * scale).round() / scale;
    let (x, y) = (snap(bounds.origin.x), snap(bounds.origin.y));
    [
        x,
        y,
        snap(bounds.origin.x + bounds.size.width) - x,
        snap(bounds.origin.y + bounds.size.height) - y,
    ]
}

/// Show the page at `bounds`, or park it if none of that is in sight.
fn place(
    native: &NativeHost,
    bounds: Bounds<Pixels>,
    visible: Bounds<Pixels>,
    scale: f32,
    radius: Pixels,
    corners: Corners,
) {
    let (whole, visible) = (rect(bounds, scale), rect(visible, scale));
    native.set_size(whole[2], whole[3]);
    if visible[2] >= 1.0 && visible[3] >= 1.0 {
        native.place(whole, visible, f64::from(f32::from(radius)), corners);
    } else {
        native.park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_that_is_cut_off_has_square_corners() {
        let both = Corners {
            top: true,
            bottom: true,
        };
        // All of it in sight.
        assert_eq!(both.mask(100., 200., 100., 200.), 0b1111);
        // Scrolled under the top of the list: only the bottom is its own.
        assert_eq!(both.mask(100., 200., 150., 150.), 0b0011);
        // Cut off below: only the top.
        assert_eq!(both.mask(100., 200., 100., 120.), 0b1100);
        // A title sits over it: its top is never rounded.
        let under_a_title = Corners {
            top: false,
            bottom: true,
        };
        assert_eq!(under_a_title.mask(100., 200., 100., 200.), 0b0011);
    }
}
