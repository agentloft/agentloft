//! The thread's device floating over the conversation (ADR-0020): its live
//! screen (touch and type in it as in the panel), moved by its bar like the
//! mini browser, and resized from any edge or corner. The screen keeps the
//! device's shape: the card's height follows its width. Its size and place
//! are the window's, kept while it runs.

use super::{DraggedPreview, PREVIEW_CARD_W, WorkspaceView, keep_preview_in};
use crate::panel::device::FLOAT_ROOM;
use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, CursorStyle, DragMoveEvent, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Point, Render, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use std::cell::Cell;
use std::rc::Rc;

/// The screen area's width before the card was resized.
pub(super) const DEVICE_PREVIEW_W: f32 = 260.;
/// The card's narrowest and widest (borders included).
const MIN_CARD_W: f32 = 150.;
const MAX_CARD_W: f32 = 1000.;
/// The bar, its border and the card's border: what the card adds to the
/// device's area.
const BAR_H: f32 = 30.;
const CHROME_W: f32 = 2.;
const CHROME_H: f32 = BAR_H + 1. + 2.;
/// How far into the card its resize handles reach.
const GRIP: f32 = 6.;
const CORNER: f32 = 14.;

/// The card's height for its width, and back: the device's screen keeps
/// its shape (`aspect`: height per width) with `room` around it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CardShape {
    pub aspect: f32,
    pub room: f32,
}

impl CardShape {
    pub fn height(&self, card_w: f32) -> f32 {
        (card_w - CHROME_W - self.room).max(0.) * self.aspect + self.room + CHROME_H
    }

    pub fn width(&self, card_h: f32) -> f32 {
        (card_h - CHROME_H - self.room).max(0.) / self.aspect.max(0.01) + self.room + CHROME_W
    }

    /// `card_w` within the card's limits, and small enough for the whole
    /// card to fit in `room` (when known): the room wins over the limits.
    pub fn fit(&self, card_w: f32, room: (f32, f32)) -> f32 {
        let mut w = card_w.clamp(MIN_CARD_W, MAX_CARD_W);
        if room.0 > 0. && room.1 > 0. {
            w = w.min(room.0).min(self.width(room.1));
        }
        w.max(1.)
    }
}

/// Which sides of the card a handle moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Edges {
    const fn new(left: bool, right: bool, top: bool, bottom: bool) -> Self {
        Self {
            left,
            right,
            top,
            bottom,
        }
    }

    /// Every side and corner, with its id.
    const ALL: [(&'static str, Edges); 8] = [
        ("device-card-l", Edges::new(true, false, false, false)),
        ("device-card-r", Edges::new(false, true, false, false)),
        ("device-card-t", Edges::new(false, false, true, false)),
        ("device-card-b", Edges::new(false, false, false, true)),
        ("device-card-tl", Edges::new(true, false, true, false)),
        ("device-card-tr", Edges::new(false, true, true, false)),
        ("device-card-bl", Edges::new(true, false, false, true)),
        ("device-card-br", Edges::new(false, true, false, true)),
    ];

    fn cursor(self) -> CursorStyle {
        match (self.left || self.right, self.top || self.bottom) {
            (true, false) => CursorStyle::ResizeLeftRight,
            (false, true) => CursorStyle::ResizeUpDown,
            _ if (self.left && self.top) || (self.right && self.bottom) => {
                CursorStyle::ResizeUpLeftDownRight
            }
            _ => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// The card resized from `edges` by a drag of `moved` (pointer pixels),
/// from `start` (`x, y, width, height` in the conversation): its height
/// follows its width, and the opposite side or corner stays where it was.
pub(crate) fn resize_card(
    start: (f32, f32, f32, f32),
    edges: Edges,
    moved: (f32, f32),
    shape: CardShape,
    room: (f32, f32),
) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = start;
    let across = if edges.right {
        Some(w + moved.0)
    } else if edges.left {
        Some(w - moved.0)
    } else {
        None
    };
    let down = if edges.bottom {
        Some(shape.width(h + moved.1))
    } else if edges.top {
        Some(shape.width(h - moved.1))
    } else {
        None
    };
    // A corner follows whichever way the pointer went further.
    let wanted = match (across, down) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => w,
    };
    let new_w = shape.fit(wanted, room);
    let new_h = shape.height(new_w);
    // The side opposite the one pulled stays where it was (a side pulled
    // alone keeps the top, or the left, too).
    let new_x = if edges.left { x + w - new_w } else { x };
    let new_y = if edges.top { y + h - new_h } else { y };
    (new_x, new_y, new_w, new_h)
}

/// A resize handle being dragged: which sides, the card's place and size
/// when it started, and where the pointer was.
#[derive(Clone)]
pub(crate) struct DraggedCardEdge {
    edges: Edges,
    start: (f32, f32, f32, f32),
    from: Rc<Cell<Option<Point<Pixels>>>>,
}

impl Render for DraggedCardEdge {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::Empty
    }
}

impl WorkspaceView {
    /// The shape of the floating device's card, if one floats.
    fn device_card_shape(&self, cx: &gpui_kit::App) -> Option<CardShape> {
        let thread_id = self.store.read(cx).selected.clone()?;
        let view = self.panel.read(cx).floating_device(&thread_id)?;
        Some(CardShape {
            aspect: view.read(cx).screen_aspect(),
            room: FLOAT_ROOM,
        })
    }

    fn room(&self) -> (f32, f32) {
        let room = self.preview_room.get();
        (f32::from(room.width), f32::from(room.height))
    }

    /// A resize handle of the floating device moved.
    pub(super) fn resize_device_card(
        &mut self,
        e: &DragMoveEvent<DraggedCardEdge>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = e.drag(cx).clone();
        let Some(from) = drag.from.get() else {
            return;
        };
        let Some(shape) = self.device_card_shape(cx) else {
            return;
        };
        let room = (
            f32::from(e.bounds.size.width),
            f32::from(e.bounds.size.height),
        );
        let at = e.event.position;
        let moved = (f32::from(at.x - from.x), f32::from(at.y - from.y));
        let (x, y, w, h) = resize_card(drag.start, drag.edges, moved, shape, room);
        self.device_preview_w = Some(w - CHROME_W);
        let at = gpui_kit::point(px(room.0 - x - w), px(room.1 - y - h));
        self.device_preview_at = Some(keep_preview_in(
            at,
            e.bounds.size,
            gpui_kit::size(px(w), px(h)),
        ));
        cx.notify();
    }

    /// The thread's device floating over the conversation, beside the mini
    /// browser when that shows too.
    pub(super) fn device_preview_card(
        &mut self,
        p: &Palette,
        beside_browser: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.device_preview || self.device_in_panel(cx) {
            return None;
        }
        let thread_id = self.store.read(cx).selected.clone()?;
        let view = self.panel.read(cx).floating_device(&thread_id)?;
        view.read(cx).chosen()?;
        let shape = self.device_card_shape(cx)?;
        let (title, busy) = {
            let v = view.read(cx);
            (v.title(cx), v.is_busy())
        };
        view.update(cx, |v, _| v.set_floating(true));
        let room = self.room();
        let wanted = self.device_preview_w.unwrap_or(DEVICE_PREVIEW_W) + CHROME_W;
        let card_w = shape.fit(wanted, room);
        let card_h = shape.height(card_w);
        let card = gpui_kit::size(px(card_w), px(card_h));
        let at = match self.device_preview_at {
            Some(at) => at,
            None if beside_browser => gpui_kit::point(px(20. + PREVIEW_CARD_W + 12.), px(150.)),
            None => gpui_kit::point(px(20.), px(150.)),
        };
        let at = keep_preview_in(at, self.preview_room.get(), card);
        // The card's place in the conversation, for its handles.
        let start = (
            room.0 - f32::from(at.x) - card_w,
            room.1 - f32::from(at.y) - card_h,
            card_w,
            card_h,
        );
        let bar_button = |id: &'static str, icon: IconName| {
            div()
                .id(id)
                .p(px(3.))
                .rounded(px(5.))
                .cursor_pointer()
                .text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                .child(Icon::new(icon).xsmall())
                .claims_press()
        };
        let handles = Edges::ALL.into_iter().map(|(id, edges)| {
            let drag = DraggedCardEdge {
                edges,
                start,
                from: Rc::default(),
            };
            let corner = (edges.left || edges.right) && (edges.top || edges.bottom);
            let full = || gpui_kit::Length::from(gpui_kit::relative(1.));
            let side = |v: f32| gpui_kit::Length::from(px(v));
            let (w, h) = match (edges.left || edges.right, corner) {
                (_, true) => (side(CORNER), side(CORNER)),
                (true, false) => (side(GRIP), full()),
                (false, false) => (full(), side(GRIP)),
            };
            let mut handle = div()
                .id(id)
                .absolute()
                .w(w)
                .h(h)
                .cursor(edges.cursor())
                .occlude()
                .claims_press()
                .on_drag(drag, |drag, _, window, cx| {
                    drag.from.set(Some(window.mouse_position()));
                    cx.new(|_| drag.clone())
                });
            handle = if edges.right {
                handle.right_0()
            } else {
                handle.left_0()
            };
            handle = if edges.bottom {
                handle.bottom_0()
            } else {
                handle.top_0()
            };
            handle.into_any_element()
        });
        Some(
            v_flex()
                .id("device-card")
                .absolute()
                .right(at.x)
                .bottom(at.y)
                .w(card.width)
                .h(card.height)
                .rounded(px(12.))
                .overflow_hidden()
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .shadow_lg()
                .child(
                    h_flex()
                        .id("device-preview-bar")
                        .h(px(BAR_H))
                        .flex_none()
                        .px_2()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .border_b_1()
                        .border_color(p.border)
                        .cursor(CursorStyle::OpenHand)
                        .on_drag(DraggedPreview::new(card, true), |d, grab, _, cx| {
                            d.grab.set(grab);
                            cx.new(|_| d.clone())
                        })
                        .child(if busy {
                            Spinner::new()
                                .xsmall()
                                .color(p.muted_foreground)
                                .into_any_element()
                        } else {
                            Icon::new(IconName::Smartphone)
                                .xsmall()
                                .text_color(p.muted_foreground)
                                .into_any_element()
                        })
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .child(
                            bar_button("device-preview-expand", IconName::Maximize2)
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "Open in the side panel",
                                    )
                                    .build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.device_preview = false;
                                    this.panel_open = true;
                                    if let Some(t) = this.store.read(cx).selected.clone() {
                                        this.panel.update(cx, |p, cx| {
                                            p.show_floated_device(&t, window, cx)
                                        });
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            bar_button("device-preview-close", IconName::X)
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "Close (the device keeps running)",
                                    )
                                    .build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.device_preview = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(view),
                )
                .children(handles)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: CardShape = CardShape {
        aspect: 2.,
        room: 26.,
    };

    fn close(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01
            && (a.1 - b.1).abs() < 0.01
            && (a.2 - b.2).abs() < 0.01
            && (a.3 - b.3).abs() < 0.01
    }

    #[test]
    fn the_height_follows_the_width_and_back() {
        let h = PHONE.height(262.);
        // 262 − 2 − 26 = 234 of screen, twice as tall, plus the room and bar.
        assert!((h - (234. * 2. + 26. + 33.)).abs() < 0.01);
        assert!((PHONE.width(h) - 262.).abs() < 0.01);
    }

    #[test]
    fn dragging_the_right_edge_widens_it_and_keeps_its_top_left() {
        let start = (100., 50., 262., PHONE.height(262.));
        let room = (2000., 2000.);
        let (x, y, w, h) = resize_card(start, Edges::ALL[1].1, (40., 0.), PHONE, room);
        assert_eq!((x, y), (100., 50.));
        assert!((w - 302.).abs() < 0.01);
        assert!((h - PHONE.height(302.)).abs() < 0.01);
    }

    #[test]
    fn dragging_the_top_left_corner_keeps_the_bottom_right() {
        let start = (300., 300., 262., PHONE.height(262.));
        let room = (2000., 2000.);
        let end = |r: (f32, f32, f32, f32)| (r.0 + r.2, r.1 + r.3);
        let r = resize_card(start, Edges::ALL[4].1, (-30., -100.), PHONE, room);
        assert!((end(r).0 - end(start).0).abs() < 0.01);
        assert!((end(r).1 - end(start).1).abs() < 0.01);
        // Up by 100 is further than left by 30 (the height grows twice as
        // fast): the corner follows the height.
        assert!((r.3 - (start.3 + 100.)).abs() < 0.01);
        // The top edge alone: the width follows, the bottom and the left
        // stay.
        let t = resize_card(start, Edges::ALL[2].1, (0., -60.), PHONE, room);
        assert!((t.3 - (start.3 + 60.)).abs() < 0.01);
        assert!(close(
            (t.0, end(t).1, 0., 0.),
            (start.0, end(start).1, 0., 0.)
        ));
    }

    #[test]
    fn sizes_stay_within_the_limits_and_the_room() {
        let start = (0., 0., 262., PHONE.height(262.));
        // Shrunk past the smallest.
        let (_, _, w, _) = resize_card(start, Edges::ALL[1].1, (-500., 0.), PHONE, (2000., 2000.));
        assert_eq!(w, MIN_CARD_W);
        // Grown past the largest.
        let (_, _, w, _) = resize_card(start, Edges::ALL[1].1, (5000., 0.), PHONE, (9000., 9000.));
        assert_eq!(w, MAX_CARD_W);
        // Grown past the room: as tall as the room, no taller.
        let (_, _, _, h) = resize_card(start, Edges::ALL[3].1, (0., 5000.), PHONE, (2000., 700.));
        assert!((h - 700.).abs() < 0.01);
        // A room too small for the smallest card: the room wins.
        let w = PHONE.fit(300., (120., 900.));
        assert_eq!(w, 120.);
    }
}
