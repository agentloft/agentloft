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
//! While it is shown the page is an ordinary web view under the pointer:
//! clicks, the keyboard, hover and the pointer's shape are the system's
//! and WebKit's own business, with nothing of ours in between.
//!
//! macOS only (WKWebView). Elsewhere [`ViewHost::new`] fails and the owner
//! falls back to the daemon's streamed page.

use crate::native::NativeHost;
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui_kit::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, Window, px, relative,
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
    /// `CACornerMask` for a layer whose y axis points down (`flipped`), or
    /// up.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn mask(self, flipped: bool) -> usize {
        const MIN_Y: usize = 0b0011;
        const MAX_Y: usize = 0b1100;
        let (top, bottom) = if flipped {
            (MIN_Y, MAX_Y)
        } else {
            (MAX_Y, MIN_Y)
        };
        let mut mask = 0;
        if self.top {
            mask |= top;
        }
        if self.bottom {
            mask |= bottom;
        }
        mask
    }
}

/// A picture of a hosted page, at the display's density. Its pixels are
/// still in the system's image: reading them out copies every one, which is
/// for a background thread to do ([`HostPicture::into_bgra`]).
pub struct HostPicture {
    pub width: u32,
    pub height: u32,
    read: Box<dyn FnOnce() -> Option<Vec<u8>> + Send>,
}

impl HostPicture {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn new(
        width: u32,
        height: u32,
        read: impl FnOnce() -> Option<Vec<u8>> + Send + 'static,
    ) -> Self {
        Self {
            width,
            height,
            read: Box::new(read),
        }
    }

    /// The pixels as GPUI draws them: blue, green, red, alpha; rows from
    /// the top, with nothing between them. `None` if the system's image
    /// turned out not to be readable.
    pub fn into_bgra(self) -> Option<Vec<u8>> {
        (self.read)()
    }
}

/// `width` × `height` pixels out of `bytes`, whose rows are `row` bytes
/// apart (they may be padded), as BGRA with nothing between the rows.
/// `order` says where blue, green, red and alpha sit in each pixel now.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn packed_bgra(
    bytes: &[u8],
    row: usize,
    order: [u8; 4],
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let line = width.checked_mul(4)?;
    if width == 0 || height == 0 || row < line || bytes.len() < row * (height - 1) + line {
        return None;
    }
    let mut pixels = Vec::with_capacity(line * height);
    if row == line {
        pixels.extend_from_slice(&bytes[..line * height]);
    } else {
        for y in 0..height {
            pixels.extend_from_slice(&bytes[y * row..y * row + line]);
        }
    }
    if order != [0, 1, 2, 3] {
        let [b, g, r, a] = order.map(usize::from);
        for pixel in pixels.as_chunks_mut::<4>().0 {
            *pixel = [pixel[b], pixel[g], pixel[r], pixel[a]];
        }
    }
    Some(pixels)
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
        // What only this world can do: post to us. The bridge is told that
        // the user has the page itself in front of them (not pictures of
        // it). And no menu of the web view's own (its "Reload" would empty
        // the page).
        let post = "globalThis.__agentloft = (message) => \
                    window.webkit.messageHandlers.agentloftView.postMessage(String(message));\
                    globalThis.__agentloftReal = true;\
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
            clear_below: Pixels::ZERO,
        }
    }

    /// Show the page now at `bounds`, as much of it as is inside `clip`
    /// (window coordinates), without waiting for the next paint: for a
    /// press that is to reach it.
    pub fn show_now(
        &self,
        bounds: Bounds<Pixels>,
        clip: Bounds<Pixels>,
        scale: f32,
        radius: Pixels,
        corners: Corners,
    ) {
        place(&self.native, bounds, clip, scale, radius, corners);
    }

    /// Whether the page is in sight right now.
    pub fn is_shown(&self) -> bool {
        self.native.is_shown()
    }

    /// Take the page out of sight now (it keeps running).
    pub fn park(&self) {
        self.native.park();
    }

    /// Which ways the wheel has something to turn in the page where the
    /// pointer is (`[left, right, up, down]`, as its bridge says). The
    /// wheel is the page's there: sideways always, up and down once it was
    /// clicked into. Everywhere else it is the thread's.
    pub fn set_reach(&self, reach: [bool; 4]) {
        self.native.set_reach(reach);
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

    /// The pointer moved with that press still held: the drag is the
    /// page's as well.
    pub fn take_drag(&self) {
        self.native.take_drag();
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

    /// Keys as the system would send them: each of `keys` pressed and
    /// released, with Command held if `command` (dev hooks).
    pub fn type_keys(&self, keys: &str, command: bool) {
        self.native.type_keys(keys, command);
    }

    /// A turn of the wheel over `(x, y)`, `lines` down (dev hooks).
    pub fn wheel_at(&self, x: f32, y: f32, lines: i32) {
        self.native.wheel_at(f64::from(x), f64::from(y), lines);
    }
}

/// Shows a hosted page where this element is painted (while it is to be
/// shown), cut off where whatever clips the element ends; takes it out of
/// sight on the first frame the element isn't painted.
pub struct HostSurface {
    native: Rc<NativeHost>,
    shown: bool,
    radius: Pixels,
    corners: Corners,
    clear_below: Pixels,
}

impl HostSurface {
    /// Leave this much at the bottom of whatever clips the element to what
    /// the app draws there (a fade, a button floating over the list): the
    /// page isn't shown in that band, and its picture is.
    pub fn keep_clear_below(mut self, band: Pixels) -> Self {
        self.clear_below = band;
        self
    }
}

/// `clip` without a band of `below` at its bottom.
pub fn without_band(mut clip: Bounds<Pixels>, below: Pixels) -> Bounds<Pixels> {
    clip.size.height = (clip.size.height - below).max(Pixels::ZERO);
    clip
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
        if self.shown {
            place(
                &self.native,
                bounds,
                without_band(window.content_mask().bounds, self.clear_below),
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

/// Show the page at `bounds` as far as `clip` reaches, or park it if none
/// of it is inside.
fn place(
    native: &NativeHost,
    bounds: Bounds<Pixels>,
    clip: Bounds<Pixels>,
    scale: f32,
    radius: Pixels,
    corners: Corners,
) {
    let whole = rect(bounds, scale);
    native.set_size(whole[2], whole[3]);
    let visible = bounds.intersect(&clip);
    if visible.size.width >= px(1.) && visible.size.height >= px(1.) {
        native.place(
            whole,
            rect(clip, scale),
            f64::from(f32::from(radius)),
            corners,
        );
    } else {
        native.park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_follow_the_way_the_layer_counts() {
        let under_a_title = Corners {
            top: false,
            bottom: true,
        };
        // Min-Y is the top of a layer that counts down, the bottom of one
        // that counts up.
        assert_eq!(under_a_title.mask(true), 0b1100);
        assert_eq!(under_a_title.mask(false), 0b0011);
        let both = Corners {
            top: true,
            bottom: true,
        };
        assert_eq!(both.mask(true), 0b1111);
        assert_eq!(Corners::default().mask(true), 0);
    }

    #[test]
    fn pixels_are_packed_and_put_in_order() {
        // Two pixels a row, rows padded to twelve bytes, as R G B A.
        let bytes = [
            1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, //
            9, 10, 11, 12, 13, 14, 15, 16,
        ];
        assert_eq!(
            packed_bgra(&bytes, 12, [2, 1, 0, 3], 2, 2).unwrap(),
            [3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
        // Already in order, nothing between the rows: as it is.
        let bytes: Vec<u8> = (0..16).collect();
        assert_eq!(packed_bgra(&bytes, 8, [0, 1, 2, 3], 2, 2).unwrap(), bytes);
        // Shorter than it says it is.
        assert!(packed_bgra(&bytes, 8, [0, 1, 2, 3], 2, 3).is_none());
        assert!(packed_bgra(&bytes, 4, [0, 1, 2, 3], 2, 2).is_none());
    }
}
