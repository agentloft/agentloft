//! The browser's own paste, for the composer.
//!
//! In a browser, ⌘V is not a key GPUI binds (gpui-kit binds `cmd-v` only when
//! built for macOS): the browser pastes, and its `paste` event reaches the
//! focused text input as `InputHandler::paste`, images included. gpui-kit's
//! text input keeps only the text of it. So the composer's input handler is
//! wrapped: everything goes to the textarea as before, except a paste the
//! composer takes itself (images, a big block of text).

use gpui_kit::component::input::TextareaState;
use gpui_kit::{
    App, Bounds, ClipboardItem, ElementInputHandler, Entity, Focusable as _, InputHandler,
    IntoElement, Pixels, Point, Styled as _, TextInputConfiguration, UTF16Selection, Window,
    canvas,
};
use std::ops::Range;
use std::rc::Rc;

/// Takes a paste (true) or leaves it to the textarea (false).
pub type OnPaste = Rc<dyn Fn(&ClipboardItem, &mut Window, &mut App) -> bool>;

/// Put right after the textarea it serves: painted later, its input handler
/// is the one the window hands the browser.
pub fn hook(state: Entity<TextareaState>, on_paste: OnPaste) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, cx| {
            let (bounds, focus) = {
                let s = state.read(cx);
                (s.text_bounds(), s.focus_handle(cx))
            };
            // Bounds the textarea painted with this frame, as its own handler.
            let Some(bounds) = bounds else {
                return;
            };
            let inner = ElementInputHandler::new(bounds, state);
            window.handle_input(&focus, Composer { inner, on_paste }, cx);
        },
    )
    .absolute()
    .size_0()
}

struct Composer {
    inner: ElementInputHandler<TextareaState>,
    on_paste: OnPaste,
}

impl InputHandler for Composer {
    fn paste(&mut self, item: ClipboardItem, window: &mut Window, cx: &mut App) {
        if !(self.on_paste)(&item, window, cx) {
            self.inner.paste(item, window, cx);
        }
    }

    // The rest is the textarea's own.

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.inner
            .selected_text_range(ignore_disabled_input, window, cx)
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.inner.marked_text_range(window, cx)
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.inner
            .text_for_range(range_utf16, adjusted_range, window, cx)
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner
            .replace_text_in_range(replacement_range, text, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.replace_and_mark_text_in_range(
            range_utf16,
            new_text,
            new_selected_range,
            window,
            cx,
        );
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.inner.unmark_text(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.inner.bounds_for_range(range_utf16, window, cx)
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<Bounds<Pixels>> {
        self.inner
            .selection_bounds_for_range(range_utf16, window, cx)
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.inner.character_index_for_point(point, window, cx)
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.set_selected_text_range(range_utf16, window, cx);
    }

    fn element_bounds(&mut self, window: &mut Window, cx: &mut App) -> Option<Bounds<Pixels>> {
        self.inner.element_bounds(window, cx)
    }

    fn text_length_utf16(&mut self, window: &mut Window, cx: &mut App) -> Option<usize> {
        self.inner.text_length_utf16(window, cx)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        self.inner.apple_press_and_hold_enabled()
    }

    fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.accepts_text_input(window, cx)
    }

    fn text_input_editable_range(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Range<usize>> {
        self.inner.text_input_editable_range(window, cx)
    }

    fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.prefers_ime_for_printable_keys(window, cx)
    }

    fn text_input_configuration(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> TextInputConfiguration {
        self.inner.text_input_configuration(window, cx)
    }
}
