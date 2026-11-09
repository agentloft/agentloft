//! `/context` and `/usage` as cards in the timeline: what fills the context
//! window, and the plan's limits, this session's cost and tips. The data
//! is Claude's own (`get_context_usage`, `get_usage`).

use crate::store::{AppStore, CardState, InfoCard, ThreadState};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px, relative, rgb,
};
use loft_protocol::AgentInfoKind;
use serde_json::Value;

/// "178.7k", "1M", "32".
pub(crate) fn tokens(n: f64) -> String {
    let trim = |s: String| s.trim_end_matches(".0").to_owned();
    if n >= 1_000_000. {
        format!("{}M", trim(format!("{:.1}", n / 1_000_000.)))
    } else if n >= 1_000. {
        format!("{}k", trim(format!("{:.1}", n / 1_000.)))
    } else {
        format!("{}", n as i64)
    }
}

/// "Sat 6:00 AM" (within the week), else "Nov 21, 6:00 AM".
fn reset_time(iso: &str) -> Option<String> {
    use chrono::{Local, TimeZone as _};
    let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let local = Local.from_utc_datetime(&at.naive_utc());
    let days = (local - Local::now()).num_days();
    Some(if (0..6).contains(&days) {
        local.format("%a %-I:%M %p").to_string()
    } else {
        local.format("%b %-d, %-I:%M %p").to_string()
    })
}

fn category_color(id: &str, kind: &str, p: &Palette) -> Hsla {
    match (id, kind) {
        (_, "free") => p.muted,
        (_, "deferred") | (_, "buffer") => rgb(0x5E5E66).into(),
        ("messages", _) => p.accent,
        ("mcp_tools", _) => rgb(0xE8692F).into(),
        ("system_tools", _) => rgb(0x14A38B).into(),
        ("memory_files", _) => rgb(0xD4930F).into(),
        _ => rgb(0x9A9AA0).into(),
    }
}

fn num(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.)
}

/// A labelled bar filled to `fraction`.
fn meter(fraction: f32, color: Hsla, p: &Palette) -> AnyElement {
    div()
        .h(px(6.))
        .w_full()
        .rounded_full()
        .bg(p.muted)
        .child(
            div()
                .h_full()
                .rounded_full()
                .w(relative(fraction.clamp(0., 1.)))
                .bg(color),
        )
        .into_any_element()
}

fn section_title(text: &str, p: &Palette) -> AnyElement {
    div()
        .pt_2()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.foreground)
        .child(text.to_owned())
        .into_any_element()
}

/// A name … value row.
fn stat_row(name: String, value: String, extra: Option<String>, p: &Palette) -> AnyElement {
    h_flex()
        .gap_3()
        .py(px(3.))
        .text_sm()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.muted_foreground)
                .child(name),
        )
        .child(div().text_color(p.muted_foreground).child(value))
        .children(extra.map(|e| {
            div()
                .w(px(52.))
                .flex()
                .justify_end()
                .text_color(p.foreground)
                .child(e)
        }))
        .into_any_element()
}

fn context_body(info: &Value, open: bool, p: &Palette) -> (String, Vec<AnyElement>) {
    let max = num(info, "maxTokens").max(1.);
    let total = num(info, "totalTokens");
    let pct = num(info, "percentage");
    let summary = format!("{} / {} ({}%)", tokens(total), tokens(max), pct as i64);
    let cats: Vec<&Value> = info
        .get("categories")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut body = Vec::new();
    // The bar: what is used (and held back), in order.
    let mut bar = h_flex()
        .h(px(8.))
        .w_full()
        .gap(px(2.))
        .rounded_full()
        .overflow_hidden();
    for c in &cats {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or("used");
        if kind == "deferred" || kind == "free" {
            continue;
        }
        let share = (num(c, "tokens") / max) as f32;
        if share <= 0. {
            continue;
        }
        let id = c.get("id").and_then(Value::as_str).unwrap_or("");
        bar = bar.child(
            div()
                .h_full()
                .w(relative(share))
                .rounded(px(2.))
                .bg(category_color(id, kind, p)),
        );
    }
    body.push(
        div()
            .w_full()
            .rounded_full()
            .bg(p.muted)
            .child(bar)
            .into_any_element(),
    );
    if !open {
        return (summary, body);
    }
    let mut list = v_flex().pt_1();
    for c in &cats {
        let kind = c.get("kind").and_then(Value::as_str).unwrap_or("used");
        let id = c.get("id").and_then(Value::as_str).unwrap_or("");
        let name = c
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let t = num(c, "tokens");
        let share = if kind == "deferred" {
            "—".to_owned()
        } else {
            let v = t / max * 100.;
            if v > 0. && v < 0.1 {
                "<0.1%".to_owned()
            } else {
                format!("{v:.1}%")
            }
        };
        list = list.child(
            h_flex()
                .gap_2()
                .py(px(3.))
                .text_sm()
                .child(
                    div()
                        .flex_none()
                        .size(px(10.))
                        .rounded(px(3.))
                        .bg(category_color(id, kind, p)),
                )
                .child(div().flex_1().min_w_0().truncate().child(name))
                .child(div().text_color(p.muted_foreground).child(tokens(t)))
                .child(div().w(px(52.)).flex().justify_end().child(share)),
        );
    }
    body.push(list.into_any_element());
    // What the bigger buckets hold.
    let mut groups = v_flex().pt_1().border_t_1().border_color(p.border);
    let mut any = false;
    for (key, label) in [
        ("mcpTools", "MCP tools"),
        ("memoryFiles", "Memory files"),
        ("agents", "Agents"),
    ] {
        let Some(list) = info
            .get(key)
            .and_then(Value::as_array)
            .filter(|l| !l.is_empty())
        else {
            continue;
        };
        any = true;
        let sum: f64 = list.iter().map(|v| num(v, "tokens")).sum();
        groups = groups.child(stat_row(
            label.to_owned(),
            tokens(sum),
            Some(list.len().to_string()),
            p,
        ));
    }
    if let Some(skills) = info.get("skills").filter(|s| num(s, "totalSkills") > 0.) {
        any = true;
        groups = groups.child(stat_row(
            "Skills".into(),
            tokens(num(skills, "tokens")),
            Some((num(skills, "totalSkills") as i64).to_string()),
            p,
        ));
    }
    if any {
        body.push(groups.into_any_element());
    }
    (summary, body)
}

fn limit_label(limit: &Value) -> String {
    let kind = limit.get("kind").and_then(Value::as_str).unwrap_or("");
    let model = limit
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str);
    let group = limit.get("group").and_then(Value::as_str).unwrap_or("");
    let span = match group {
        "weekly" => "Weekly",
        "five_hour" | "session" => "Current session",
        "" => "Limit",
        other => other,
    };
    match (kind, model) {
        (_, Some(m)) => format!("{span} · {m}"),
        (k, None) if k.ends_with("_all") => format!("{span} · all models"),
        _ => span.to_owned(),
    }
}

fn usage_body(info: &Value, p: &Palette) -> (String, Vec<AnyElement>) {
    let mut body = Vec::new();
    let plan = info
        .get("subscription_type")
        .and_then(Value::as_str)
        .map(|s| {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let limits: Vec<&Value> = info
        .pointer("/rate_limits/limits")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if limits.is_empty() {
        body.push(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child("Plan limits aren't available for this sign-in.")
                .into_any_element(),
        );
    }
    for limit in limits {
        let pct = num(limit, "percent") as f32;
        let severe = limit.get("severity").and_then(Value::as_str) != Some("normal");
        let reset = limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(reset_time)
            .map(|t| format!("Resets {t}"))
            .unwrap_or_default();
        body.push(
            v_flex()
                .gap_1p5()
                .py_1()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(p.muted_foreground)
                                .child(limit_label(limit)),
                        )
                        .child(div().text_xs().text_color(p.faint_foreground).child(reset))
                        .child(
                            div()
                                .w(px(40.))
                                .flex()
                                .justify_end()
                                .font_weight(FontWeight::MEDIUM)
                                .child(format!("{}%", pct as i64)),
                        ),
                )
                .child(meter(
                    pct / 100.,
                    if severe { p.warning } else { p.accent },
                    p,
                ))
                .into_any_element(),
        );
    }
    // This session.
    if let Some(session) = info.get("session") {
        body.push(section_title("This session", p));
        let cost = num(session, "total_cost_usd");
        let api = num(session, "total_api_duration_ms") / 1000.;
        let added = num(session, "total_lines_added") as i64;
        let removed = num(session, "total_lines_removed") as i64;
        body.push(stat_row("Cost".into(), format!("${cost:.2}"), None, p));
        body.push(stat_row(
            "Time the model worked".into(),
            crate::items::human_duration((api * 1000.) as i64),
            None,
            p,
        ));
        if added + removed > 0 {
            body.push(stat_row(
                "Lines changed".into(),
                format!("+{added} −{removed}"),
                None,
                p,
            ));
        }
        if let Some(models) = session.get("model_usage").and_then(Value::as_object)
            && !models.is_empty()
        {
            body.push(section_title("By model", p));
            for (model, u) in models {
                let parts: Vec<String> = [
                    ("inputTokens", "in"),
                    ("outputTokens", "out"),
                    ("cacheReadInputTokens", "cache read"),
                    ("cacheCreationInputTokens", "cache write"),
                ]
                .iter()
                .filter(|(key, _)| u.get(*key).is_some())
                .map(|(key, label)| format!("{label} {}", tokens(num(u, key))))
                .collect();
                let name = model.strip_prefix("claude-").unwrap_or(model).to_owned();
                body.push(stat_row(name, parts.join(" · "), None, p));
            }
        }
    }
    // What is using the limits, and what to do about it.
    let tips: Vec<&Value> = info
        .pointer("/behaviors/day/recommendations")
        .and_then(Value::as_array)
        .map(|a| a.iter().take(4).collect())
        .unwrap_or_default();
    if !tips.is_empty() {
        body.push(section_title("What's using your limits (last 24h)", p));
        for tip in tips {
            let title = tip.get("title").and_then(Value::as_str).unwrap_or("");
            let detail = tip.get("detail").and_then(Value::as_str);
            body.push(
                h_flex()
                    .gap_3()
                    .py(px(3.))
                    .items_start()
                    .text_sm()
                    .child(
                        div()
                            .w(px(40.))
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!("{}%", num(tip, "pct") as i64)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_color(p.foreground).child(title.to_owned()))
                            .children(detail.map(|d| {
                                div()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(d.to_owned())
                            })),
                    )
                    .into_any_element(),
            );
        }
    }
    (plan, body)
}

fn icon_button(
    id: String,
    icon: IconName,
    tip: &'static str,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(SharedString::from(id))
        .p(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(p.muted_foreground)
        .hover(|d| d.bg(p.hover).text_color(p.foreground))
        .child(Icon::new(icon).small())
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
        })
}

/// Card `ix` of `state`.
pub(crate) fn render(
    thread: &Entity<ThreadState>,
    store: &Entity<AppStore>,
    state: &ThreadState,
    ix: usize,
    p: &Palette,
) -> AnyElement {
    let Some(InfoCard {
        kind,
        state: card,
        open,
        ..
    }) = state.cards.get(ix)
    else {
        return div().into_any_element();
    };
    let (title, command, icon) = match kind {
        AgentInfoKind::Context => ("Context window", "/context", IconName::Gauge),
        AgentInfoKind::Usage => ("Usage", "/usage", IconName::ChartPie),
    };
    let (summary, body): (String, Vec<AnyElement>) = match card {
        CardState::Loading => (
            String::new(),
            vec![
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(Spinner::new().xsmall().color(p.muted_foreground))
                    .child("Asking the agent…")
                    .into_any_element(),
            ],
        ),
        CardState::Failed(msg) => (
            String::new(),
            vec![
                div()
                    .text_sm()
                    .text_color(p.danger)
                    .child(msg.clone())
                    .into_any_element(),
            ],
        ),
        CardState::Ready(info) => match kind {
            AgentInfoKind::Context => context_body(info, *open, p),
            AgentInfoKind::Usage => usage_body(info, p),
        },
    };
    let foldable = matches!(kind, AgentInfoKind::Context) && matches!(card, CardState::Ready(_));
    let thread_id = state.id.clone();
    let (t_fold, t_reload, t_close) = (thread.clone(), thread.clone(), thread.clone());
    let store_reload = store.clone();
    let compact = matches!(kind, AgentInfoKind::Usage).then(|| {
        let (store, id) = (store.clone(), thread_id.clone());
        h_flex().pt_2().gap_2().child(
            div()
                .id(SharedString::from(format!("card-compact-{ix}")))
                .px_3()
                .py_1p5()
                .rounded(px(8.))
                .bg(p.muted)
                .text_sm()
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child("Compact this session")
                .on_click(move |_, _, cx| {
                    let id = id.clone();
                    store.update(cx, |s, cx| {
                        s.send_message(id, "/compact".into(), Vec::new(), cx)
                    });
                }),
        )
    });
    v_flex()
        .gap_1()
        .pt_3()
        .child(
            h_flex().justify_end().child(
                div()
                    .px_3()
                    .py_1()
                    .rounded(px(10.))
                    .bg(p.muted)
                    .text_sm()
                    .text_color(p.accent)
                    .child(command),
            ),
        )
        .child(
            v_flex()
                .w_full()
                .max_w(px(620.))
                .gap_2()
                .px_4()
                .py_3()
                .rounded(px(12.))
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .child(
                    h_flex()
                        .id(SharedString::from(format!("card-head-{ix}")))
                        .gap_2()
                        .items_center()
                        .when(foldable, |d| d.cursor_pointer())
                        .child(Icon::new(icon).small().text_color(p.muted_foreground))
                        .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(title))
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted_foreground)
                                .child(summary),
                        )
                        .when(foldable, |d| {
                            d.child(
                                Icon::new(if *open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .text_color(p.muted_foreground),
                            )
                            .on_click(move |_, _, cx| {
                                t_fold.update(cx, |t, cx| t.toggle_card(ix, cx));
                            })
                        })
                        .child(
                            icon_button(
                                format!("card-reload-{ix}"),
                                IconName::RotateCw,
                                "Refresh",
                                p,
                            )
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let client = store_reload.read(cx).client();
                                t_reload.update(cx, |t, cx| t.load_card(ix, client, cx));
                            }),
                        )
                        .child(
                            icon_button(format!("card-close-{ix}"), IconName::X, "Dismiss", p)
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    t_close.update(cx, |t, cx| t.remove_card(ix, cx));
                                }),
                        ),
                )
                .children(body)
                .children(compact),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(178_700.), "178.7k");
        assert_eq!(tokens(1_000_000.), "1M");
        assert_eq!(tokens(33_000.), "33k");
        assert_eq!(tokens(32.), "32");
    }

    #[test]
    fn limit_labels() {
        let all: Value = serde_json::json!({"kind": "weekly_all", "group": "weekly"});
        assert_eq!(limit_label(&all), "Weekly · all models");
        let fable: Value = serde_json::json!({"kind": "weekly_scoped", "group": "weekly",
            "scope": {"model": {"display_name": "Fable"}}});
        assert_eq!(limit_label(&fable), "Weekly · Fable");
    }
}
