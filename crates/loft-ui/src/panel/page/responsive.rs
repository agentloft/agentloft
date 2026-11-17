//! Responsive mode: the tab shown at a size of its own rather than the
//! panel's. A strip under the toolbar picks a screen or takes a width and
//! height; the page sits centred with handles on its sides and corners that
//! resize it by dragging. The size is the tab's (the daemon's page), so the
//! web client and agents see the page at it too.

use super::PageView;
use super::tools::{Anchor, DEVICES};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, CursorStyle, DragMoveEvent, Entity,
    Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point,
    Render, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px,
};
use loft_browser::menu::{self, Entry};
use loft_protocol::{PageDevice, Request};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// The strip's height.
pub(super) const STRIP_HEIGHT: f32 = 34.;
/// How far from the page's edge its resize handles reach.
const RAIL: f32 = 10.;
/// The room left around a page of its own size (its handles live there).
pub(super) const MARGIN: f32 = 16.;
/// The name of a size that is no particular screen.
const RESPONSIVE: &str = "Responsive";
/// The page's sides, as the daemon keeps them.
const MIN_SIDE: u32 = 120;
const MAX_SIDE: u32 = 3840;
/// While a handle is dragged, the size is sent at most this often.
const SEND_EVERY: Duration = Duration::from_millis(50);

/// A side or corner of the page that resizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edge {
    West,
    East,
    South,
    SouthWest,
    SouthEast,
}

impl Edge {
    const ALL: [Edge; 5] = [
        Edge::West,
        Edge::East,
        Edge::South,
        Edge::SouthWest,
        Edge::SouthEast,
    ];

    /// Which way the width (−1, 0, 1) and the height (0, 1) follow the
    /// pointer.
    fn pulls(self) -> (f32, f32) {
        match self {
            Edge::West => (-1., 0.),
            Edge::East => (1., 0.),
            Edge::South => (0., 1.),
            Edge::SouthWest => (-1., 1.),
            Edge::SouthEast => (1., 1.),
        }
    }

    fn cursor(self) -> CursorStyle {
        match self {
            Edge::West | Edge::East => CursorStyle::ResizeLeftRight,
            Edge::South => CursorStyle::ResizeUpDown,
            Edge::SouthWest => CursorStyle::ResizeUpRightDownLeft,
            Edge::SouthEast => CursorStyle::ResizeUpLeftDownRight,
        }
    }
}

/// A handle being dragged: the size the page had when it started, how
/// many view pixels a CSS pixel took then, and where the pointer was.
#[derive(Clone)]
pub(super) struct RailDrag {
    edge: Edge,
    start: (u32, u32),
    per_css: f32,
    from: Rc<Cell<Option<Point<Pixels>>>>,
}

impl Render for RailDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::Empty
    }
}

/// The size a drag of `delta` view pixels from `start` asks for. The page
/// stays centred, so its edge moves half as far as its size changes; with
/// `aspect` it keeps its proportions.
pub(super) fn dragged_size(
    start: (u32, u32),
    per_css: f32,
    edge: Edge,
    delta: (f32, f32),
    aspect: Option<f32>,
) -> (u32, u32) {
    let (pull_x, pull_y) = edge.pulls();
    let per_css = per_css.max(0.01);
    let mut w = start.0 as f32 + 2. * pull_x * delta.0 / per_css;
    let mut h = start.1 as f32 + 2. * pull_y * delta.1 / per_css;
    if let Some(aspect) = aspect.filter(|a| a.is_finite() && *a > 0.) {
        if pull_x != 0. {
            h = w / aspect;
        } else {
            w = h * aspect;
        }
    }
    let side = |v: f32| (v.round().max(0.) as u32).clamp(MIN_SIDE, MAX_SIDE);
    (side(w), side(h))
}

/// The two boxes of the strip: width and height.
pub(super) struct Dims {
    width: Entity<InputState>,
    height: Entity<InputState>,
    /// The size they show (they are filled in again when it changes, unless
    /// one is being typed in).
    shown: Option<(u32, u32)>,
    /// Kept proportions while dragging (width ÷ height), when locked.
    aspect: Option<f32>,
    preset_anchor: Anchor,
    /// The size a drag asks for, and the send waiting to ask it.
    wanted: Option<(u32, u32)>,
    sending: Option<gpui_kit::Task<()>>,
}

impl Dims {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<PageView>,
    ) -> (Self, Vec<gpui_kit::Subscription>) {
        let width = cx.new(|cx| InputState::new(window, cx));
        let height = cx.new(|cx| InputState::new(window, cx));
        let on_event = |this: &mut PageView,
                        _: &Entity<InputState>,
                        event: &InputEvent,
                        window: &mut Window,
                        cx: &mut Context<PageView>| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.apply_typed_size(window, cx);
            }
        };
        let subs = vec![
            cx.subscribe_in(&width, window, on_event),
            cx.subscribe_in(&height, window, on_event),
        ];
        (
            Self {
                width,
                height,
                shown: None,
                aspect: None,
                preset_anchor: Rc::default(),
                wanted: None,
                sending: None,
            },
            subs,
        )
    }
}

impl PageView {
    pub(super) fn device_info(&self) -> Option<&PageDevice> {
        self.info.as_ref()?.device.as_ref()
    }

    pub fn is_responsive(&self) -> bool {
        self.device_info().is_some()
    }

    /// Turn responsive mode on (at the size the panel gives the page now)
    /// or off (the page fills the panel again).
    pub fn set_responsive(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let device = on.then(|| {
            let (w, h) = self.bounds.get().map_or((1280., 800.), |b| {
                (
                    f32::from(b.size.width) - 2. * MARGIN,
                    f32::from(b.size.height) - STRIP_HEIGHT - 2. * MARGIN,
                )
            });
            let side = |v: f32| ((v / info.zoom).round().max(0.) as u32).clamp(MIN_SIDE, MAX_SIDE);
            PageDevice {
                name: RESPONSIVE.into(),
                width: side(w),
                height: side(h),
                mobile: false,
                user_agent: None,
            }
        });
        let id = info.id.clone();
        self.dims.aspect = None;
        self.page_request(Request::PageEmulate { id, device }, cx);
    }

    /// Show the page at `size`, keeping the kind of screen it is shown as
    /// (a phone stays a phone), but as no particular one.
    fn set_size(&mut self, size: (u32, u32), cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        let Some(current) = &info.device else {
            return;
        };
        if (current.width, current.height) == size {
            return;
        }
        let device = PageDevice {
            name: RESPONSIVE.into(),
            width: size.0,
            height: size.1,
            ..current.clone()
        };
        let id = info.id.clone();
        self.page_request(
            Request::PageEmulate {
                id,
                device: Some(device),
            },
            cx,
        );
    }

    /// Ask for `size` soon: a drag asks many times a second, and only the
    /// latest matters.
    fn set_size_soon(&mut self, size: (u32, u32), cx: &mut Context<Self>) {
        self.dims.wanted = Some(size);
        if self.dims.sending.is_some() {
            return;
        }
        self.dims.sending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEND_EVERY).await;
            let _ = this.update(cx, |this, cx| {
                this.dims.sending = None;
                if let Some(size) = this.dims.wanted.take() {
                    this.set_size(size, cx);
                }
            });
        }));
    }

    fn apply_typed_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let read = |input: &Entity<InputState>, cx: &Context<Self>| {
            input.read(cx).value().trim().parse::<u32>().ok()
        };
        let (Some(w), Some(h)) = (read(&self.dims.width, cx), read(&self.dims.height, cx)) else {
            // Not a number: show the size it has again.
            self.dims.shown = None;
            cx.notify();
            return;
        };
        let size = (w.clamp(MIN_SIDE, MAX_SIDE), h.clamp(MIN_SIDE, MAX_SIDE));
        self.set_size(size, cx);
        if size != (w, h) {
            self.dims.shown = None;
        }
        let _ = window;
        cx.notify();
    }

    fn rotate(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.device_info().cloned() else {
            return;
        };
        let Some(info) = &self.info else {
            return;
        };
        let id = info.id.clone();
        self.dims.aspect = self.dims.aspect.map(|a| 1. / a);
        let turned = PageDevice {
            width: device.height,
            height: device.width,
            ..device
        };
        self.page_request(
            Request::PageEmulate {
                id,
                device: Some(turned),
            },
            cx,
        );
    }

    pub(super) fn on_rail_move(
        &mut self,
        event: &DragMoveEvent<RailDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx).clone();
        let Some(from) = drag.from.get() else {
            return;
        };
        let at = event.event.position;
        let delta = (f32::from(at.x - from.x), f32::from(at.y - from.y));
        let size = dragged_size(drag.start, drag.per_css, drag.edge, delta, self.dims.aspect);
        self.set_size_soon(size, cx);
    }

    /// The handles around the page, placed in `area` (the page's element)
    /// around `drawn` (where the page is).
    pub(super) fn rails(
        &self,
        drawn: Bounds<Pixels>,
        area: Bounds<Pixels>,
        per_css: f32,
        p: &Palette,
    ) -> Vec<AnyElement> {
        let Some(device) = self.device_info() else {
            return Vec::new();
        };
        let start = (device.width, device.height);
        let (left, top) = (
            f32::from(drawn.origin.x - area.origin.x),
            f32::from(drawn.origin.y - area.origin.y),
        );
        let (w, h) = (f32::from(drawn.size.width), f32::from(drawn.size.height));
        let grip = p.muted_foreground;
        Edge::ALL
            .into_iter()
            .map(|edge| {
                let (x, y, rw, rh) = match edge {
                    Edge::West => (left - RAIL, top, RAIL, h),
                    Edge::East => (left + w, top, RAIL, h),
                    Edge::South => (left, top + h, w, RAIL),
                    Edge::SouthWest => (left - RAIL, top + h, RAIL, RAIL),
                    Edge::SouthEast => (left + w, top + h, RAIL, RAIL),
                };
                let drag = RailDrag {
                    edge,
                    start,
                    per_css,
                    from: Rc::default(),
                };
                // A short bar in the middle of a side says it can be pulled.
                let bar = match edge {
                    Edge::West | Edge::East => Some(
                        div()
                            .w(px(3.))
                            .h(px(28.0f32.min(h)))
                            .rounded(px(2.))
                            .bg(grip.opacity(0.5)),
                    ),
                    Edge::South => Some(
                        div()
                            .h(px(3.))
                            .w(px(28.0f32.min(w)))
                            .rounded(px(2.))
                            .bg(grip.opacity(0.5)),
                    ),
                    _ => None,
                };
                div()
                    .id(("page-rail", edge as usize))
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(rw))
                    .h(px(rh))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(edge.cursor())
                    .rounded(px(3.))
                    .hover(|d| d.bg(grip.opacity(0.15)))
                    .children(bar)
                    .on_drag(drag, |drag, _, window, cx| {
                        drag.from.set(Some(window.mouse_position()));
                        cx.new(|_| drag.clone())
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// The strip under the toolbar while the page has a size of its own.
    pub(super) fn dims_strip(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let device = self.device_info()?.clone();
        let size = (device.width, device.height);
        let typing = self.dims.width.read(cx).focus_handle(cx).is_focused(window)
            || self
                .dims
                .height
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
        if !typing && self.dims.shown != Some(size) {
            self.dims.shown = Some(size);
            self.dims
                .width
                .update(cx, |s, cx| s.set_value(size.0.to_string(), window, cx));
            self.dims
                .height
                .update(cx, |s, cx| s.set_value(size.1.to_string(), window, cx));
        }
        let locked = self.dims.aspect.is_some();
        let wide = self
            .bounds
            .get()
            .is_none_or(|b| f32::from(b.size.width) >= 480.);
        Some(
            h_flex()
                .h(px(STRIP_HEIGHT))
                .flex_shrink_0()
                .px_2()
                .gap_1()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .text_xs()
                .text_color(p.muted_foreground)
                .when(wide, |d| d.child(div().mr_1().child("Dimensions")))
                .child(self.preset_menu(&device, cx))
                .child(
                    div()
                        .w(px(58.))
                        .child(Input::new(&self.dims.width).xsmall()),
                )
                .child("×")
                .child(
                    div()
                        .w(px(58.))
                        .child(Input::new(&self.dims.height).xsmall()),
                )
                .child(
                    Button::new("page-aspect")
                        .cursor_pointer()
                        .xsmall()
                        .icon(if locked {
                            IconName::Link2
                        } else {
                            IconName::Unlink2
                        })
                        .when(locked, |b| b.primary())
                        .when(!locked, |b| b.ghost())
                        .tooltip(if locked {
                            "Unlock the proportions"
                        } else {
                            "Keep the proportions while resizing"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dims.aspect = match this.device_info() {
                                Some(d) if this.dims.aspect.is_none() => {
                                    Some(d.width as f32 / d.height.max(1) as f32)
                                }
                                _ => None,
                            };
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("page-rotate")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::RotateCwSquare)
                        .tooltip("Rotate")
                        .on_click(cx.listener(|this, _, _, cx| this.rotate(cx))),
                )
                .child(div().flex_1())
                .child(
                    Button::new("page-responsive-off")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::X)
                        .tooltip("Fill the panel again")
                        .on_click(cx.listener(|this, _, _, cx| this.set_responsive(false, cx))),
                )
                .into_any_element(),
        )
    }

    /// The screen picker: Responsive, or one of [`DEVICES`].
    fn preset_menu(&self, device: &PageDevice, cx: &mut Context<Self>) -> AnyElement {
        let current = DEVICES.iter().position(|d| d.name == device.name);
        let label = current.map_or(RESPONSIVE, |ix| DEVICES[ix].name);
        let view = cx.weak_entity();
        let anchor = self.dims.preset_anchor.clone();
        let button = Button::new("page-preset")
            .cursor_pointer()
            .ghost()
            .xsmall()
            .label(label)
            .icon(IconName::ChevronDown)
            .disabled(self.info.is_none())
            .on_click(move |_, window, cx| {
                let v = view.clone();
                let mut entries = vec![
                    Entry::item(RESPONSIVE.to_owned(), IconName::Expand, move |cx| {
                        _ = v.update(cx, |this, cx| this.set_responsive(true, cx));
                    })
                    .checked(current.is_none()),
                    Entry::Separator,
                ];
                let mut last = None;
                for (ix, d) in DEVICES.iter().enumerate() {
                    if last.is_some_and(|k| k != d.kind) {
                        entries.push(Entry::Separator);
                    }
                    last = Some(d.kind);
                    let v = view.clone();
                    entries.push(
                        Entry::item(
                            format!("{} · {}×{}", d.name, d.width, d.height),
                            d.icon(),
                            move |cx| {
                                _ = v.update(cx, |this, cx| this.set_device(Some(ix), false, cx));
                            },
                        )
                        .checked(current == Some(ix)),
                    );
                }
                if let Some(at) = anchor.get() {
                    menu::open(window, at, entries, cx);
                }
            });
        div()
            .relative()
            .child(button)
            .child({
                let at = self.dims.preset_anchor.clone();
                canvas(move |bounds, _, _| at.set(Some(bounds)), |_, _, _, _| {})
                    .absolute()
                    .size_full()
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_resize_a_centred_page() {
        // Its own size: pulling the right side out 10 px widens it by 20.
        assert_eq!(
            dragged_size((800, 600), 1.0, Edge::East, (10., 99.), None),
            (820, 600)
        );
        // The left side pulls the other way.
        assert_eq!(
            dragged_size((800, 600), 1.0, Edge::West, (10., 0.), None),
            (780, 600)
        );
        // Shown at half size, a view pixel is two of the page's.
        assert_eq!(
            dragged_size((800, 600), 0.5, Edge::South, (0., 10.), None),
            (800, 640)
        );
        // A corner does both; locked proportions follow the width.
        assert_eq!(
            dragged_size(
                (800, 600),
                1.0,
                Edge::SouthEast,
                (50., 5.),
                Some(800. / 600.)
            ),
            (900, 675)
        );
        // Never smaller than the daemon keeps a page.
        assert_eq!(
            dragged_size((200, 200), 1.0, Edge::West, (500., 0.), None),
            (MIN_SIDE, 200)
        );
    }
}
