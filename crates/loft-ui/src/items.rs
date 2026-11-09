//! Rendering of timeline items (user/assistant messages, reasoning, tool
//! calls with inline approval cards, notices, errors).

use crate::store::{AppStore, ThreadState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{
    DecisionKind, ItemBody, ItemStatus, NoticeLevel, RequestStatus, RuntimeRequest,
};

const MAX_OUTPUT_LINES: usize = 8;

pub fn render_item(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    ix: usize,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let state = thread.read(cx);
    let Some(model) = state.model.as_ref() else {
        return div().into_any_element();
    };
    let Some(item) = model.items.get(ix) else {
        return div().into_any_element();
    };
    let row = v_flex().w_full().py_2();

    match &item.body {
        ItemBody::UserMessage { text } => row
            .items_end()
            .child(
                div()
                    .max_w(px(640.))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.secondary)
                    .child(text.clone()),
            )
            .into_any_element(),
        ItemBody::AssistantMessage { text } => {
            let body = match state.markdown.get(&item.id) {
                Some(md) => TextView::new(md).stream_fade(true).into_any_element(),
                None => {
                    TextView::markdown(SharedString::from(format!("md-{}", item.id)), text.clone())
                        .into_any_element()
                }
            };
            row.child(body).into_any_element()
        }
        ItemBody::Reasoning { text } => {
            let preview: String = text.lines().take(4).collect::<Vec<_>>().join("\n");
            row.child(
                v_flex()
                    .pl_3()
                    .border_l_2()
                    .border_color(theme.border)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .italic()
                            .child(if item.status == ItemStatus::InProgress {
                                "Thinking…"
                            } else {
                                "Thought"
                            }),
                    )
                    .when(!preview.is_empty(), |d| d.child(preview)),
            )
            .into_any_element()
        }
        ItemBody::ToolCall {
            tool,
            title,
            output,
            is_error,
            ..
        } => {
            let title = relative_to(title, &model.thread.cwd);
            let (glyph, color) = match item.status {
                ItemStatus::InProgress => ("◌", theme.info),
                ItemStatus::Completed if !*is_error => ("✓", theme.success),
                ItemStatus::Declined => ("⊘", theme.warning),
                _ => ("✕", theme.danger),
            };
            let request = model.request_for_item(&item.id).cloned();
            let mut card = v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_color(color).child(glyph))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(tool_label(tool)),
                        )
                        .child(div().flex_1().truncate().font_family("Menlo").child(title)),
                );
            if let Some(out) = output.as_ref().filter(|o| !o.trim().is_empty()) {
                let lines: Vec<&str> = out.lines().collect();
                let shown = lines
                    .iter()
                    .take(MAX_OUTPUT_LINES)
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                let more = lines.len().saturating_sub(MAX_OUTPUT_LINES);
                card = card.child(
                    div()
                        .font_family("Menlo")
                        .text_xs()
                        .text_color(if *is_error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(shown)
                        .when(more > 0, |d| d.child(format!("… {more} more lines"))),
                );
            }
            if let Some(req) = request.filter(|r| r.status == RequestStatus::Open) {
                card = card.child(approval_card(&req, store, cx));
            }
            row.child(card).into_any_element()
        }
        ItemBody::Notice { level, text } => row
            .child(
                div()
                    .text_xs()
                    .text_color(match level {
                        NoticeLevel::Info => theme.muted_foreground,
                        NoticeLevel::Warning => theme.warning,
                    })
                    .child(text.clone()),
            )
            .into_any_element(),
        ItemBody::Error { text } => row
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.danger)
                    .text_sm()
                    .text_color(theme.danger)
                    .child(text.clone()),
            )
            .into_any_element(),
    }
}

fn approval_card(req: &RuntimeRequest, store: &Entity<AppStore>, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let mut buttons = h_flex().gap_2().pt_1();
    for decision in &req.options {
        let decision = *decision;
        let store = store.clone();
        let thread_id = req.thread_id.clone();
        let request_id = req.id.clone();
        let mut button = Button::new(SharedString::from(format!("{}-{:?}", req.id, decision)))
            .label(decision.label())
            .xsmall();
        button = match decision {
            DecisionKind::AllowOnce => button.primary(),
            DecisionKind::AllowSession => button.outline(),
            DecisionKind::Deny => button.outline(),
            DecisionKind::DenyAndStop => button.danger(),
        };
        buttons = buttons.child(button.on_click(move |_, _, cx| {
            let (thread_id, request_id) = (thread_id.clone(), request_id.clone());
            store.update(cx, |s, cx| s.respond(thread_id, request_id, decision, cx));
        }));
    }
    v_flex()
        .mt_1()
        .p_2()
        .gap_1()
        .rounded_md()
        .bg(theme.muted)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Allow {}?", req.title)),
        )
        .child(buttons)
        .into_any_element()
}

/// Short display name for a tool (`mcp__server__tool` → `server·tool`).
fn tool_label(tool: &str) -> String {
    match tool.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", "·", 1),
        None => tool.to_owned(),
    }
}

/// Show paths inside the thread's working directory relative to it.
fn relative_to(text: &str, cwd: &str) -> String {
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    let mut out = text.replace(&prefix, "");
    // macOS canonical paths: /private/tmp ↔ /tmp, /private/var ↔ /var.
    if let Some(short) = prefix.strip_prefix("/private") {
        out = out.replace(short, "");
    }
    out
}
