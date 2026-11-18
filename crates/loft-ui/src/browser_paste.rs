//! The browser's own paste, for the composer.
//!
//! In a browser, ⌘V and Ctrl+V are not keys GPUI binds in a text field
//! (gpui-kit binds `cmd-v` only when built for macOS, and [`key_bindings`]
//! takes back the `ctrl-v` it binds everywhere else): the browser pastes,
//! and its `paste` event reaches the focused text input as
//! `InputHandler::paste`, images included, with no clipboard permission
//! asked. gpui-kit's text input keeps only the text of it. So the composer's
//! input handler is wrapped: everything goes to the textarea as before,
//! except a paste the composer takes itself (images, a big block of text).

use gpui_kit::component::input::TextareaState;
use gpui_kit::{
    App, Bounds, ClipboardItem, ElementInputHandler, Entity, Focusable as _, InputHandler,
    IntoElement, KeyBinding, NoAction, Pixels, Point, Styled as _, TextInputConfiguration,
    UTF16Selection, Window, canvas,
};
use std::ops::Range;
use std::rc::Rc;

/// gpui-kit's text fields' key context.
const INPUT_CONTEXT: &str = "Input";

/// Keys a text field leaves to the browser, so that the browser's own
/// `paste` event follows them. Bound after gpui-kit's own bindings.
///
/// gpui-kit binds `ctrl-v` to `Paste` in every build but macOS's, the
/// browser's included. A key GPUI handles is cancelled by its web platform
/// (`preventDefault` on the `keydown`), so no `paste` event came, and
/// `Paste` could only read through the asynchronous clipboard read, which
/// asks for the clipboard permission. Unbound, Ctrl+V is the browser's
/// paste, as ⌘V is. A field's context menu Paste still reads that way (its
/// click allows the read; no key can do it for the menu).
pub fn key_bindings() -> Vec<KeyBinding> {
    bindings_for(crate::host::IS_WEB)
}

fn bindings_for(web: bool) -> Vec<KeyBinding> {
    if !web {
        return Vec::new();
    }
    vec![KeyBinding::new("ctrl-v", NoAction, Some(INPUT_CONTEXT))]
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::input::Paste;
    use gpui_kit::{Action as _, KeyContext, Keymap, Keystroke};

    /// What Ctrl+V runs in a text field, with gpui-kit's binding as every
    /// build but macOS's has it, then ours.
    fn ctrl_v_in_a_field(web: bool) -> Vec<&'static str> {
        let mut bindings = vec![KeyBinding::new("ctrl-v", Paste, Some(INPUT_CONTEXT))];
        bindings.extend(bindings_for(web));
        let keymap = Keymap::new(bindings);
        let field = [
            KeyContext::parse("Workspace").unwrap(),
            KeyContext::parse(INPUT_CONTEXT).unwrap(),
        ];
        let (matched, pending) =
            keymap.bindings_for_input(&[Keystroke::parse("ctrl-v").unwrap()], &field);
        assert!(!pending);
        matched.iter().map(|b| b.action().name()).collect()
    }

    #[test]
    fn ctrl_v_in_a_browser_is_the_browsers_paste() {
        assert_eq!(ctrl_v_in_a_field(true), Vec::<&str>::new());
    }

    #[test]
    fn ctrl_v_on_the_desktop_stays_paste() {
        assert_eq!(ctrl_v_in_a_field(false), vec![Paste.name()]);
    }
}
