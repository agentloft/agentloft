//! The GPUI element that hosts the native web view.
//!
//! Native views draw above GPUI's Metal layer, so the WKWebView must exactly
//! track the element and vanish whenever the element is not painted. Each
//! paint (1) applies the element's bounds, rounded to device pixels, and
//! (2) stores a [`PaintGuard`] as element state. GPUI keeps element state
//! only for elements painted in the current frame: on the first frame
//! without this element (a tab switched away, a panel closed) the previous
//! frame's state is dropped and the guard hides the view. Cached views keep
//! their element state, so they stay visible exactly as long as they are
//! drawn.

use crate::native::NativeWebView;
use gpui_kit::{
    App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, MouseDownEvent, Pixels, Style, Window, relative,
};
use std::panic::Location;
use std::rc::{Rc, Weak};

pub(crate) struct Surface {
    native: Rc<NativeWebView>,
    /// The owner's `set_visible` flag.
    visible: bool,
    /// Page zoom to apply while painted here.
    zoom: f64,
}

impl Surface {
    pub fn new(native: Rc<NativeWebView>, visible: bool) -> Self {
        Self {
            native,
            visible,
            zoom: 1.0,
        }
    }

    pub fn zoom(mut self, zoom: f64) -> Self {
        self.zoom = zoom;
        self
    }
}

/// Hides the native view when dropped, unless a newer paint superseded it.
struct PaintGuard {
    token: u64,
    native: Weak<NativeWebView>,
}

impl PaintGuard {
    fn new(native: &Rc<NativeWebView>) -> Self {
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
        // If the element moved in the tree it got a new id and a new guard
        // in the same frame; only the latest guard may hide the view.
        if let Some(native) = self.native.upgrade()
            && native.paint_token.get() == self.token
        {
            native.set_hidden(true);
        }
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some("loft-browser-surface".into())
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
        let mut style = Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..Style::default()
        };
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

        let clip = window.content_mask().bounds;
        let visible_area = bounds.intersect(&clip);
        let show = self.visible
            && f32::from(visible_area.size.width) >= 1.0
            && f32::from(visible_area.size.height) >= 1.0;
        if show {
            let scale = f64::from(window.scale_factor());
            let snap = |v: Pixels| (f64::from(f32::from(v)) * scale).round() / scale;
            let x = snap(bounds.origin.x);
            let y = snap(bounds.origin.y);
            let w = snap(bounds.origin.x + bounds.size.width) - x;
            let h = snap(bounds.origin.y + bounds.size.height) - y;
            self.native.set_frame(x, y, w, h);
            self.native.set_page_zoom(self.zoom);
        }
        self.native.set_hidden(!show);

        // A click anywhere else in the window returns key focus to GPUI.
        // (Clicks inside the web view go to WebKit and never reach GPUI.)
        let native = Rc::downgrade(&self.native);
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, _| {
            if phase == DispatchPhase::Capture
                && !bounds.contains(&event.position)
                && let Some(native) = native.upgrade()
            {
                native.release_focus();
            }
        });
    }
}
