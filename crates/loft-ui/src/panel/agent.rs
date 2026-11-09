//! Subagent view: one Task/Agent tool call's own activity — its prompt, the
//! tools it ran, its messages, and the report it returned.

use crate::items::{render_item, tool_line};
use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{ItemBody, ItemId, ItemStatus, ThreadId};

pub struct AgentView {
    store: Entity<AppStore>,
    pub thread_id: ThreadId,
    pub item_id: ItemId,
    report: Option<(String, Entity<TextViewState>)>,
    show_prompt: bool,
}

impl AgentView {
    pub fn new(
        store: Entity<AppStore>,
        thread_id: ThreadId,
        item_id: ItemId,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-render whenever the thread changes (live subagent progress).
        if let Some(thread) = store.read(cx).threads.get(&thread_id).cloned() {
            cx.observe(&thread, |_, _, cx| cx.notify()).detach();
        }
        Self {
            store,
            thread_id,
            item_id,
            report: None,
            show_prompt: false,
        }
    }

    /// Short title for the panel tab.
    /// The agent running the thread (its logo marks the subagent).
    pub fn harness(&self, cx: &gpui_kit::App) -> Option<String> {
        let store = self.store.read(cx);
        let thread = store.threads.get(&self.thread_id)?.read(cx);
        Some(thread.model.as_ref()?.thread.harness.as_str().to_owned())
    }

    pub fn title(&self, cx: &gpui_kit::App) -> String {
        let store = self.store.read(cx);
        store
            .threads
            .get(&self.thread_id)
            .and_then(|t| t.read(cx).model.as_ref())
            .and_then(|m| m.items.iter().find(|i| i.id == self.item_id))
            .and_then(|i| match &i.body {
                ItemBody::ToolCall { title, .. } => Some(title.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "Agent".into())
    }
}

impl Render for AgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let Some(thread) = self.store.read(cx).threads.get(&self.thread_id).cloned() else {
            return div().into_any_element();
        };
        // Pull out what we need, then build the report markdown if it changed.
        let (agent, report_text) = {
            let state = thread.read(cx);
            let Some(model) = state.model.as_ref() else {
                return div().into_any_element();
            };
            let Some(agent) = model.items.iter().find(|i| i.id == self.item_id).cloned() else {
                return div().into_any_element();
            };
            let report = match &agent.body {
                ItemBody::ToolCall { output, .. } => output
                    .as_deref()
                    .map(clean_report)
                    .filter(|o| !o.trim().is_empty()),
                _ => None,
            };
            (agent, report)
        };
        if let Some(text) = report_text {
            let stale = self.report.as_ref().is_none_or(|(t, _)| *t != text);
            if stale {
                let md = cx.new(|cx| TextViewState::markdown(&text, cx).selectable(true));
                self.report = Some((text, md));
            }
        }

        let ItemBody::ToolCall { title, input, .. } = &agent.body else {
            return div().into_any_element();
        };
        let running = agent.status == ItemStatus::InProgress;
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let kind = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("general-purpose")
            .to_owned();

        let state = thread.read(cx);
        let model = state.model.as_ref().expect("checked above");
        let cwd = model.thread.cwd.clone();
        let mut activity = v_flex().gap_2();
        let mut count = 0;
        for item in model
            .items
            .iter()
            .filter(|i| i.parent_id.as_ref() == Some(&self.item_id))
        {
            count += 1;
            activity = activity.child(match &item.body {
                ItemBody::ToolCall { .. } => {
                    tool_line(&thread, &self.store, state, model, item, &p)
                }
                _ => render_item(&thread, state, item, &p),
            });
        }

        let header = h_flex()
            .gap_3()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .size(px(32.))
                    .rounded_full()
                    .border_1()
                    .border_color(p.border)
                    .bg(p.elevated)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(crate::brand::agent_logo(
                        model.thread.harness.as_str(),
                        px(17.),
                        &p,
                    )),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(format!(
                                "{kind} agent · {count} step{} · {}",
                                if count == 1 { "" } else { "s" },
                                if running { "working" } else { "finished" }
                            )),
                    ),
            )
            .when(running, |d| d.child(Spinner::new().small().color(p.accent)));

        let prompt_block = (!prompt.is_empty()).then(|| {
            let open = self.show_prompt;
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .id("agent-prompt")
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .cursor_pointer()
                        .hover(|d| d.text_color(p.foreground))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall(),
                        )
                        .child("Task given to the agent")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_prompt = !this.show_prompt;
                            cx.notify();
                        })),
                )
                .when(open, |d| {
                    d.child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded(px(8.))
                            .bg(p.muted)
                            .text_sm()
                            .child(prompt.clone()),
                    )
                })
        });

        let report = self.report.as_ref().map(|(_, md)| {
            v_flex()
                .gap_1()
                .pt_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.muted_foreground)
                        .child("REPORT"),
                )
                .child(TextView::new(md).image_source(crate::items::local_images(cwd.clone())))
        });

        let _ = window;
        div()
            .id("agent-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .child(header)
                    .children(prompt_block)
                    .child(activity)
                    .when(count == 0 && running, |d| {
                        d.child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child("Waiting for the agent's first step…"),
                        )
                    })
                    .children(report),
            )
            .into_any_element()
    }
}

/// Strip Claude Code's subagent hand-back wrapper: the framing preamble,
/// `<tools-used-before-summary>`/`<usage>` tags, the `agentId:` footer and
/// the two-space indentation the harness adds to every report line.
pub fn clean_report(raw: &str) -> String {
    fn strip_tag(text: &str, tag: &str) -> String {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        let mut out = text.to_owned();
        while let Some(start) = out.find(&open) {
            let Some(end) = out[start..].find(&close) else {
                break;
            };
            out.replace_range(start..start + end + close.len(), "");
        }
        out
    }
    // A background launch only returns a receipt; the report comes later.
    if raw.trim_start().starts_with("Async agent launched") {
        return String::new();
    }
    let mut text = strip_tag(&strip_tag(raw, "tools-used-before-summary"), "usage");
    const MARKER: &str = "The report follows:";
    if text.contains("[Subagent hand-back]")
        && let Some(ix) = text.find(MARKER)
    {
        text = text[ix + MARKER.len()..].to_owned();
    }
    let lines: Vec<&str> = text
        .lines()
        .take_while(|l| !l.starts_with("agentId:"))
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .collect();
    lines.join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::clean_report;

    #[test]
    fn strips_subagent_wrapper() {
        let raw = "  <tools-used-before-summary>Bash</tools-used-before-summary>\n  \n[Subagent hand-back] blah. The report follows:\n  I ran `ls`.\n  \n  **7 entries**\nagentId: a1 (use SendMessage)\n<usage>subagent_tokens: 1\nduration_ms: 2</usage>";
        assert_eq!(clean_report(raw), "I ran `ls`.\n\n**7 entries**");
        assert_eq!(clean_report("plain report"), "plain report");
    }
}
