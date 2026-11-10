//! Small pieces of UI for what differs between the desktop app and the
//! browser (see [`crate::host`]).

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, v_flex};
use gpui_kit::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, div};

/// Fills a panel the browser build can't offer: what it is, and that it is
/// not available here.
pub fn not_in_browser(icon: IconName, what: &str, p: &Palette) -> AnyElement {
    unavailable(icon, what, crate::host::NOT_IN_BROWSER, p)
}

/// A quiet, centered note in place of a panel's content.
pub fn unavailable(icon: IconName, title: &str, detail: &str, p: &Palette) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .p_6()
        .child(Icon::new(icon).large().text_color(p.faint_foreground))
        .child(
            div()
                .text_sm()
                .text_color(p.foreground)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(detail.to_owned()),
        )
        .into_any_element()
}
