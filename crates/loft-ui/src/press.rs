//! Controls drawn by hand (a `div` with `on_click`) and the window's text
//! selection, which starts on any press, blank space included.

use gpui_kit::{InteractiveElement, MouseButton};

/// For a hand-drawn control: a press on it is the control's and starts no
/// text selection, as gpui-kit's own `Button` does. Needed wherever the
/// click stops propagation: that also stops the mouse-up from reaching a
/// selection the press started, which would then follow the pointer with
/// no button held.
pub(crate) trait ClaimsPress: InteractiveElement + Sized {
    fn claims_press(self) -> Self {
        self.on_mouse_down(MouseButton::Left, |_, _, cx| {
            gpui_kit::component::GlobalState::suppress_text_selection(cx)
        })
    }
}

impl<T: InteractiveElement> ClaimsPress for T {}
