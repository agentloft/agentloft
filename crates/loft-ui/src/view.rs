//! Views an agent draws in the chat (`show_ui`, ADR-0018).
//!
//! A view is a page of the agent's own HTML. The daemon runs it in a tab of
//! its browser and streams it here, the same way the web client's browser
//! tab works ([`crate::panel::page`]); this module is the frame around it in
//! the timeline: which items are views, the card each sits in, and the look
//! a page is told to match.

use crate::embed::EmbedView;
use crate::panel::page::PageView;
use crate::store::{AppStore, Row, ThreadState};
use crate::theme::Palette;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::v_flex;
use gpui_kit::{
    AnyElement, App, AppContext as _, Entity, FontWeight, Hsla, IntoElement, ParentElement as _,
    Styled as _, Window, div, px,
};
use loft_protocol::{Item, ItemBody, ItemStatus, ViewTheme};

/// How a view is on screen: the real page in a web view of the app's own,
/// where the platform has one ([`EmbedView`]), or pictures of the page the
/// daemon runs ([`PageView`]: a browser client, a system without a native
/// web view).
#[derive(Clone)]
pub(crate) enum Shown {
    Page(Entity<PageView>),
    Embed(Entity<EmbedView>),
}

impl Shown {
    fn element(&self) -> AnyElement {
        match self {
            Shown::Page(page) => page.clone().into_any_element(),
            Shown::Embed(embed) => embed.clone().into_any_element(),
        }
    }

    /// Where the view is drawn in its window, once it has been.
    pub(crate) fn bounds(&self, cx: &App) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        match self {
            Shown::Page(page) => page.read(cx).bounds(),
            Shown::Embed(embed) => embed.read(cx).bounds(),
        }
    }

    fn flush_top(&self, cx: &App) -> std::rc::Rc<std::cell::Cell<bool>> {
        match self {
            Shown::Page(page) => page.read(cx).flush_top(),
            Shown::Embed(embed) => embed.read(cx).flush_top(),
        }
    }
}

/// The agent's HTML in a view's item as it is now, and whether that is all
/// of it (a call still being written says it isn't).
fn html_of(item: &Item) -> (&str, bool) {
    match &item.body {
        ItemBody::ToolCall { input, .. } => (
            input["html"].as_str().unwrap_or_default(),
            !input[loft_view::PARTIAL].as_bool().unwrap_or(false),
        ),
        _ => ("", true),
    }
}

/// Whether a timeline item is a view: a `show_ui` call that went through
/// (or is still being written). One that was turned down stays an ordinary
/// failed tool line, so what the agent got wrong can be read.
pub(crate) fn is_view(item: &Item) -> bool {
    matches!(&item.body, ItemBody::ToolCall { tool, is_error, .. }
        if loft_view::is_tool(tool)
            && !is_error
            && matches!(item.status, ItemStatus::InProgress | ItemStatus::Completed))
}

/// A colour as CSS.
fn css(colour: Hsla) -> String {
    let rgba = gpui_kit::Rgba::from(colour);
    let byte = |channel: f32| (channel.clamp(0., 1.) * 255.).round() as u8;
    let (r, g, b) = (byte(rgba.r), byte(rgba.g), byte(rgba.b));
    if rgba.a >= 0.999 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("rgba({r},{g},{b},{:.3})", rgba.a)
    }
}

/// A font the app uses, as a CSS family list the daemon's browser can
/// resolve (the system font has no name a page could ask for).
fn family(name: &str, fallback: &str) -> String {
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') || name.eq_ignore_ascii_case("system-ui") {
        fallback.to_owned()
    } else {
        format!("{name}, {fallback}")
    }
}

/// The app's look as a view is told it: the card a view sits in is the
/// page's background, so the two read as one surface.
pub(crate) fn theme(cx: &App) -> ViewTheme {
    let p = Palette::of(cx);
    let theme = cx.theme();
    ViewTheme {
        dark: p.dark,
        background: css(p.elevated),
        foreground: css(p.foreground),
        muted: css(p.muted_foreground),
        border: css(p.border),
        surface: css(p.muted),
        accent: css(p.accent),
        accent_foreground: css(p.accent_foreground),
        success: css(p.success),
        warning: css(p.warning),
        danger: css(p.danger),
        font: family(&theme.font_family, "system-ui, -apple-system, sans-serif"),
        mono: family(&p.mono, "ui-monospace, Menlo, monospace"),
        font_size: f32::from(theme.font_size),
    }
}

/// Make sure the view in timeline row `ix` (if it is one) has its page
/// view. Called before the row is drawn, with the window a page view needs
/// to be made.
pub(crate) fn ensure(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    ix: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let (thread_id, item_id, shown) = {
        let state = thread.read(cx);
        let (Some(model), Some(Row::Item(i))) = (state.model.as_ref(), state.rows.get(ix)) else {
            return;
        };
        let item = &model.items[*i];
        if !is_view(item) {
            return;
        }
        (state.id.clone(), item.id.clone(), state.view(&item.id))
    };
    let embed = match shown {
        Some(Shown::Embed(embed)) => embed,
        Some(Shown::Page(_)) => return,
        None => {
            let shown = make(store, &thread_id, &item_id, window, cx);
            thread.update(cx, |state, cx| {
                state.adopt_view(item_id.clone(), shown.clone(), cx)
            });
            match shown {
                Shown::Embed(embed) => embed,
                Shown::Page(_) => return,
            }
        }
    };
    // The app writes an embedded page itself: what its item has by now.
    let fresh = {
        let state = thread.read(cx);
        state
            .model
            .as_ref()
            .and_then(|model| model.items.iter().find(|item| item.id == item_id))
            .map(html_of)
            .filter(|(html, done)| !embed.read(cx).has(html, *done))
            .map(|(html, done)| (html.to_owned(), done))
    };
    if let Some((html, done)) = fresh {
        embed.update(cx, |embed, cx| embed.set_html(&html, done, cx));
    }
}

/// A view's place on screen: the real page where that can be, else pictures
/// of the daemon's.
fn make(
    store: &Entity<AppStore>,
    thread: &loft_protocol::ThreadId,
    item: &loft_protocol::ItemId,
    window: &mut Window,
    cx: &mut App,
) -> Shown {
    if crate::embed::native() {
        let (events, heard) = futures::channel::mpsc::unbounded();
        let origin = loft_view::origin(item.as_str());
        match loft_browser::ViewHost::new(window, &origin, loft_view::ALLOWED_HOSTS, events) {
            Ok(host) => {
                return Shown::Embed(cx.new(|cx| {
                    EmbedView::new(store.clone(), thread.clone(), item.clone(), host, heard, cx)
                }));
            }
            Err(err) => tracing::warn!(err = %format!("{err:#}"), "no native page for a view"),
        }
    }
    Shown::Page(
        cx.new(|cx| PageView::for_view(store.clone(), thread.clone(), item.clone(), window, cx)),
    )
}

/// The row of a view item: its title, if the agent gave one, over its page.
pub(crate) fn render(state: &ThreadState, item: &Item, p: &Palette) -> AnyElement {
    let title = match &item.body {
        ItemBody::ToolCall { input, .. } => input["title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
        _ => None,
    };
    v_flex()
        .w_full()
        .rounded(px(12.))
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .overflow_hidden()
        .when_some(title, |card, title| {
            card.child(
                div()
                    .px_4()
                    .py_2p5()
                    .border_b_1()
                    .border_color(p.border)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.foreground)
                    .child(title),
            )
        })
        .children(state.view(&item.id).map(|shown| shown.element()))
        .into_any_element()
}

/// Tell a view's page whether it is the top of its card (see
/// [`PageView::flush_top`]).
pub(crate) fn settle(state: &ThreadState, item: &Item, cx: &App) {
    if let Some(shown) = state.view(&item.id) {
        let titled = matches!(&item.body, ItemBody::ToolCall { input, .. }
            if input["title"].as_str().is_some_and(|t| !t.trim().is_empty()));
        shown.flush_top(cx).set(!titled);
    }
}

use gpui_kit::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::{ItemId, ThreadId, ToolKind};

    fn call(tool: &str, status: ItemStatus, is_error: bool) -> Item {
        Item {
            id: ItemId("i".into()),
            thread_id: ThreadId("t".into()),
            turn_id: None,
            ordinal: 0,
            status,
            body: ItemBody::ToolCall {
                tool: tool.into(),
                tool_kind: ToolKind::Mcp,
                title: tool.into(),
                input: serde_json::json!({}),
                output: None,
                is_error,
            },
            updated_at_ms: 0,
            parent_id: None,
            started_at_ms: None,
        }
    }

    #[test]
    fn only_a_show_ui_call_that_went_through_is_a_view() {
        let tool = "mcp__agentloft__show_ui";
        assert!(is_view(&call(tool, ItemStatus::Completed, false)));
        assert!(is_view(&call(tool, ItemStatus::InProgress, false)));
        // Turned down: an ordinary failed tool line.
        assert!(!is_view(&call(tool, ItemStatus::Failed, true)));
        assert!(!is_view(&call(tool, ItemStatus::Completed, true)));
        assert!(!is_view(&call(tool, ItemStatus::Declined, false)));
        // Someone else's tool of the same name isn't ours.
        assert!(!is_view(&call(
            "mcp__other__show_ui",
            ItemStatus::Completed,
            false
        )));
    }

    #[test]
    fn a_call_still_being_written_is_not_all_of_its_html() {
        let mut item = call("mcp__agentloft__show_ui", ItemStatus::InProgress, false);
        let mut say = |input: serde_json::Value| {
            if let ItemBody::ToolCall { input: slot, .. } = &mut item.body {
                *slot = input;
            }
            let (html, done) = html_of(&item);
            (html.to_owned(), done)
        };
        assert_eq!(
            say(serde_json::json!({ "html": "<p>half", "_partial": true })),
            ("<p>half".to_owned(), false)
        );
        // A harness that reports a call whole says nothing of parts.
        assert_eq!(
            say(serde_json::json!({ "html": "<p>whole</p>" })),
            ("<p>whole</p>".to_owned(), true)
        );
        assert_eq!(say(serde_json::json!({})), (String::new(), true));
    }

    #[test]
    fn colours_and_fonts_become_css() {
        assert_eq!(css(gpui_kit::rgb(0x18181b).into()), "#18181b");
        let half = Hsla::from(gpui_kit::rgb(0xffffff)).opacity(0.5);
        assert_eq!(css(half), "rgba(255,255,255,0.500)");
        assert_eq!(family(".SystemUIFont", "system-ui"), "system-ui");
        assert_eq!(family("Inter", "system-ui"), "Inter, system-ui");
        assert_eq!(family("", "monospace"), "monospace");
    }
}
