//! The drawn title bars on Windows.
//!
//! Windows open with a transparent title bar so that, on macOS, the traffic
//! lights sit over our own top strips. On Windows that removes the system's
//! title bar altogether: nothing is left to move the window by, and no
//! minimize, maximize or close buttons. So there the strips say which parts
//! are the title bar ([`MovesWindow`]) and draw the three buttons
//! ([`controls`]). Both work through Windows' own hit test (`WM_NCHITTEST`),
//! so dragging, double-clicking, Snap and the Snap layouts flyout behave as
//! they do in any Windows app.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, InteractiveElement, IntoElement as _, MouseButton, ParentElement as _, Styled as _,
    Window, WindowControlArea, div, px,
};

/// This platform needs the window's buttons drawn by us.
pub(crate) const DRAWS_CONTROLS: bool = cfg!(target_os = "windows");

/// Width of each window button, as Windows draws its own.
const BUTTON_WIDTH: f32 = 46.;

pub(crate) trait MovesWindow: InteractiveElement + Sized {
    /// Pressing this strip moves the window, as a title bar does.
    ///
    /// Controls in the strip keep their presses: they claim them
    /// ([`crate::press`]), and the strip then tells Windows the press was
    /// taken, so it doesn't start moving the window instead.
    fn moves_window(self) -> Self {
        if !DRAWS_CONTROLS {
            return self;
        }
        self.window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                if crate::press::claimed(cx) {
                    cx.stop_propagation();
                } else {
                    // Windows moves the window and keeps the mouse-up: a
                    // text selection started here would never end.
                    gpui_kit::component::GlobalState::suppress_text_selection(cx);
                }
            })
    }
}

impl<T: InteractiveElement> MovesWindow for T {}

/// Minimize, maximize (or restore) and close, for the right end of the top
/// strip, as tall as the strip. `None` where the system draws its own.
pub(crate) fn controls(window: &Window, p: &Palette) -> Option<AnyElement> {
    if !DRAWS_CONTROLS {
        return None;
    }
    let maximize = if window.is_maximized() {
        IconName::WindowRestore
    } else {
        IconName::WindowMaximize
    };
    Some(
        h_flex()
            .h_full()
            .flex_shrink_0()
            .child(button(
                "window-minimize",
                IconName::WindowMinimize,
                WindowControlArea::Min,
                p,
            ))
            .child(button(
                "window-maximize",
                maximize,
                WindowControlArea::Max,
                p,
            ))
            .child(button(
                "window-close",
                IconName::WindowClose,
                WindowControlArea::Close,
                p,
            ))
            .into_any_element(),
    )
}

/// One button. Windows does what it is for (the press goes to the area, not
/// to us), so it has no click handler of its own. It occludes the strip
/// under it, or the strip's drag area would be what Windows finds there.
fn button(
    id: &'static str,
    icon: IconName,
    area: WindowControlArea,
    p: &Palette,
) -> impl gpui_kit::IntoElement {
    let close = matches!(area, WindowControlArea::Close);
    let (hover_bg, hover_fg) = if close {
        // Windows' own close red, with a white cross.
        (gpui_kit::rgb(0xc42b1c).into(), gpui_kit::white())
    } else {
        (p.hover, p.foreground)
    };
    div()
        .id(id)
        .w(px(BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(p.muted_foreground)
        .occlude()
        .window_control_area(area)
        .hover(move |d| d.bg(hover_bg).text_color(hover_fg))
        .child(Icon::new(icon).small())
}
