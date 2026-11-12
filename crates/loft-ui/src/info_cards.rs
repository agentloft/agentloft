//! Agent reports as cards in the timeline: `/context` and `/usage` from
//! Claude (`get_context_usage`, `get_usage`), `/status` from Codex
//! (account, limits, usage history and the thread's tokens).

use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::store::{AppStore, CardState, InfoCard, ThreadState};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px, relative, rgb,
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
pub(crate) fn local_time(at: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Local;
    let local = at.with_timezone(&Local);
    let days = (local - Local::now()).num_days();
    if local.date_naive() == Local::now().date_naive() {
        local.format("%-I:%M %p").to_string()
    } else if (0..6).contains(&days) {
        local.format("%a %-I:%M %p").to_string()
    } else {
        local.format("%b %-d, %-I:%M %p").to_string()
    }
}

fn reset_time(iso: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    Some(local_time(at.with_timezone(&chrono::Utc)))
}

fn reset_unix(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(local_time)
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

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arr<'a>(v: &'a Value, path: &str) -> Vec<&'a Value> {
    v.pointer(path)
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// "syncedSkills" → "synced skills".
fn words(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out.replace(['_', '-'], " ")
}

/// "mcp__agentloft__browser_click" → "agentloft · browser_click".
fn tool_name(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
        Some((server, tool)) => format!("{} · {tool}", server.replace("claude_ai_", "claude.ai ")),
        None => name.to_owned(),
    }
}

/// "claude-opus-5-5" → "Opus 5.5".
pub(crate) fn model_name(id: &str) -> String {
    let id = id.strip_prefix("claude-").unwrap_or(id);
    let mut parts = id.split('-');
    let family = parts.next().unwrap_or(id);
    let version: Vec<&str> = parts
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    let mut name = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

/// A bar filled to `fraction`.
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

fn section_title(text: impl Into<SharedString>, p: &Palette) -> gpui_kit::Div {
    div()
        .pt_3()
        .pb_1()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.foreground)
        .child(text.into())
}

/// A name … value row with a hairline under it.
fn stat_row(name: String, value: String, extra: Option<String>, p: &Palette) -> AnyElement {
    h_flex()
        .gap_3()
        .py_1()
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .text_sm()
        .child(div().flex_none().text_color(p.muted_foreground).child(name))
        .child(
            div().flex_1().min_w_0().flex().justify_end().child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(p.muted_foreground)
                    .child(value),
            ),
        )
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

/// A limit: label, reset time, percent and its bar.
fn limit_row(
    label: String,
    reset: Option<String>,
    pct: f32,
    warn: bool,
    p: &Palette,
) -> AnyElement {
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
                        .child(label),
                )
                .children(reset.map(|r| {
                    div()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(format!("Resets {r}"))
                }))
                .child(
                    div()
                        .w(px(40.))
                        .flex()
                        .justify_end()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("{}%", pct.round() as i64)),
                ),
        )
        .child(meter(
            pct / 100.,
            if warn { p.warning } else { p.accent },
            p,
        ))
        .into_any_element()
}

/// Cells three to a row, each with a hairline under it.
fn grid(cells: Vec<String>, p: &Palette) -> AnyElement {
    let mut rows = v_flex();
    for chunk in cells.chunks(3) {
        let mut row = h_flex()
            .py_1()
            .border_b_1()
            .border_color(p.border.opacity(0.6))
            .text_sm()
            .text_color(p.muted_foreground);
        for cell in chunk {
            row = row.child(div().flex_1().min_w_0().truncate().child(cell.clone()));
        }
        for _ in chunk.len()..3 {
            row = row.child(div().flex_1());
        }
        rows = rows.child(row);
    }
    rows.into_any_element()
}

/// A small control that cycles or picks (segmented look).
fn chip(
    id: String,
    label: impl Into<SharedString>,
    on: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(SharedString::from(id))
        .px_2()
        .py(px(2.))
        .rounded(px(6.))
        .text_xs()
        .cursor_pointer()
        .when(on, |d| d.bg(p.selected).text_color(p.foreground))
        .when(!on, |d| {
            d.text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
        })
        .child(label.into())
}

/// Changes card `ix`'s controls from a click.
fn on_card(
    thread: &Entity<ThreadState>,
    ix: usize,
    f: impl Fn(&mut InfoCard) + 'static,
) -> impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static {
    let thread = thread.clone();
    move |_, _, cx| {
        cx.stop_propagation();
        thread.update(cx, |t, cx| t.card_ui(ix, &f, cx));
    }
}

/// A foldable section header: name, tokens, count.
#[allow(clippy::too_many_arguments)]
fn fold_row(
    thread: &Entity<ThreadState>,
    ix: usize,
    key: &'static str,
    label: String,
    value: String,
    count: Option<String>,
    open: bool,
    p: &Palette,
) -> AnyElement {
    h_flex()
        .id(SharedString::from(format!("card-{ix}-fold-{key}")))
        .gap_2()
        .py_1()
        .text_sm()
        .cursor_pointer()
        .text_color(p.muted_foreground)
        .hover(|d| d.text_color(p.foreground))
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall(),
        )
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(div().child(value))
        .child(
            div()
                .w(px(52.))
                .flex()
                .justify_end()
                .text_color(p.faint_foreground)
                .child(count.unwrap_or_default()),
        )
        .on_click(on_card(thread, ix, move |c| {
            match c.expanded.iter().position(|k| *k == key) {
                Some(i) => {
                    c.expanded.remove(i);
                }
                None => c.expanded.push(key),
            }
        }))
        .into_any_element()
}

/// An indented line inside an unfolded section.
fn sub_row(name: String, value: String, note: Option<&str>, p: &Palette) -> AnyElement {
    h_flex()
        .gap_2()
        .pl_5()
        .py(px(2.))
        .text_xs()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(p.muted_foreground)
                .child(name),
        )
        .children(note.map(|n| div().text_color(p.faint_foreground).child(n.to_owned())))
        .child(
            div()
                .w(px(52.))
                .flex()
                .justify_end()
                .text_color(p.muted_foreground)
                .child(value),
        )
        .into_any_element()
}

// -- /context ------------------------------------------------------------------

fn context_body(
    info: &Value,
    card: &InfoCard,
    thread: &Entity<ThreadState>,
    ix: usize,
    p: &Palette,
) -> (String, Vec<AnyElement>) {
    let max = num(info, "maxTokens").max(1.);
    let total = num(info, "totalTokens");
    let summary = format!(
        "{} / {} ({}%)",
        tokens(total),
        tokens(max),
        num(info, "percentage") as i64
    );
    let cats = arr(info, "/categories");
    let mut body = Vec::new();
    let mut bar = h_flex().h(px(8.)).w_full().gap(px(2.));
    for c in &cats {
        let kind = text(c, "kind").unwrap_or("used");
        if kind == "deferred" || kind == "free" {
            continue;
        }
        let share = (num(c, "tokens") / max) as f32;
        if share <= 0. {
            continue;
        }
        bar = bar.child(
            div()
                .h_full()
                .w(relative(share))
                .rounded(px(2.))
                .bg(category_color(text(c, "id").unwrap_or(""), kind, p)),
        );
    }
    body.push(
        div()
            .w_full()
            .rounded_full()
            .overflow_hidden()
            .bg(p.muted)
            .child(bar)
            .into_any_element(),
    );
    if !card.open {
        return (summary, body);
    }
    let mut list = v_flex().pt_1();
    for c in &cats {
        let kind = text(c, "kind").unwrap_or("used");
        let id = text(c, "id").unwrap_or("");
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
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(text(c, "name").unwrap_or(id).to_owned()),
                )
                .child(div().text_color(p.muted_foreground).child(tokens(t)))
                .child(div().w(px(52.)).flex().justify_end().child(share)),
        );
    }
    body.push(list.into_any_element());

    // What the bigger buckets hold, each foldable.
    let open = |k: &str| card.expanded.contains(&k);
    let mut more = v_flex().pt_2().border_t_1().border_color(p.border);
    // Messages.
    if let Some(m) = info.get("messageBreakdown") {
        let parts = [
            ("Your messages", num(m, "userMessageTokens")),
            ("Replies", num(m, "assistantMessageTokens")),
            ("Tool calls", num(m, "toolCallTokens")),
            ("Tool results", num(m, "toolResultTokens")),
            ("Attachments", num(m, "attachmentTokens")),
        ];
        let sum: f64 = parts.iter().map(|(_, t)| t).sum();
        more = more.child(fold_row(
            thread,
            ix,
            "messages",
            "Messages".into(),
            tokens(sum),
            None,
            open("messages"),
            p,
        ));
        if open("messages") {
            for (name, t) in parts.into_iter().filter(|(_, t)| *t > 0.) {
                more = more.child(sub_row(name.into(), tokens(t), None, p));
            }
            let mut tools = arr(m, "/toolCallsByType");
            tools.sort_by(|a, b| num(b, "tokens").total_cmp(&num(a, "tokens")));
            for t in tools.into_iter().take(8) {
                let name = text(t, "name").unwrap_or("tool");
                let total = num(t, "callTokens") + num(t, "resultTokens") + num(t, "tokens");
                more = more.child(sub_row(tool_name(name), tokens(total), Some("tool"), p));
            }
            let mut atts = arr(m, "/attachmentsByType");
            atts.sort_by(|a, b| num(b, "tokens").total_cmp(&num(a, "tokens")));
            for a in atts.into_iter().take(6) {
                let name = text(a, "name").unwrap_or("attachment").replace('_', " ");
                more = more.child(sub_row(name, tokens(num(a, "tokens")), Some("attached"), p));
            }
        }
    }
    // MCP tools, by server.
    let tools = arr(info, "/mcpTools");
    if !tools.is_empty() {
        let mut servers: Vec<(String, f64, usize, usize)> = Vec::new();
        for t in &tools {
            let server = text(t, "serverName")
                .unwrap_or("mcp")
                .replace("claude_ai_", "claude.ai ")
                .replace('_', " ");
            let loaded = t.get("isLoaded").and_then(Value::as_bool).unwrap_or(true);
            match servers.iter_mut().find(|s| s.0 == server) {
                Some(s) => {
                    s.1 += num(t, "tokens");
                    s.2 += 1;
                    s.3 += loaded as usize;
                }
                None => servers.push((server, num(t, "tokens"), 1, loaded as usize)),
            }
        }
        servers.sort_by(|a, b| b.1.total_cmp(&a.1));
        let sum: f64 = servers.iter().map(|s| s.1).sum();
        more = more.child(fold_row(
            thread,
            ix,
            "mcp",
            "MCP tools".into(),
            tokens(sum),
            Some(tools.len().to_string()),
            open("mcp"),
            p,
        ));
        if open("mcp") {
            for (server, t, n, loaded) in servers {
                let note = if loaded == n {
                    format!("{n} tools")
                } else {
                    format!("{loaded} of {n} loaded")
                };
                more = more.child(sub_row(server, tokens(t), Some(&note), p));
            }
        }
    }
    // Memory files.
    let memory = arr(info, "/memoryFiles");
    if !memory.is_empty() {
        let sum: f64 = memory.iter().map(|m| num(m, "tokens")).sum();
        more = more.child(fold_row(
            thread,
            ix,
            "memory",
            "Memory files".into(),
            tokens(sum),
            Some(memory.len().to_string()),
            open("memory"),
            p,
        ));
        if open("memory") {
            for m in memory {
                let path = text(m, "path").or(text(m, "name")).unwrap_or("file");
                more = more.child(sub_row(
                    crate::workspace::tilde_path_str(path),
                    tokens(num(m, "tokens")),
                    text(m, "type"),
                    p,
                ));
            }
        }
    }
    // Skills.
    if let Some(skills) = info.get("skills").filter(|s| num(s, "totalSkills") > 0.) {
        more = more.child(fold_row(
            thread,
            ix,
            "skills",
            "Skills".into(),
            tokens(num(skills, "tokens")),
            Some((num(skills, "totalSkills") as i64).to_string()),
            open("skills"),
            p,
        ));
        if open("skills") {
            let mut list = arr(skills, "/skillFrontmatter");
            list.sort_by(|a, b| num(b, "tokens").total_cmp(&num(a, "tokens")));
            let n = list.len();
            for s in list.iter().take(12) {
                more = more.child(sub_row(
                    text(s, "name").unwrap_or("skill").to_owned(),
                    tokens(num(s, "tokens")),
                    text(s, "source").map(words).as_deref(),
                    p,
                ));
            }
            if n > 12 {
                more = more.child(sub_row(
                    format!("and {} more", n - 12),
                    String::new(),
                    None,
                    p,
                ));
            }
        }
    }
    // Agents.
    let agents = arr(info, "/agents");
    if !agents.is_empty() {
        let sum: f64 = agents.iter().map(|a| num(a, "tokens")).sum();
        more = more.child(fold_row(
            thread,
            ix,
            "agents",
            "Agents".into(),
            tokens(sum),
            Some(agents.len().to_string()),
            open("agents"),
            p,
        ));
        if open("agents") {
            for a in agents {
                let name = text(a, "agentType").or(text(a, "name")).unwrap_or("agent");
                more = more.child(sub_row(
                    name.to_owned(),
                    tokens(num(a, "tokens")),
                    text(a, "source"),
                    p,
                ));
            }
        }
    }
    body.push(more.into_any_element());
    if info.get("isAutoCompactEnabled").and_then(Value::as_bool) == Some(true) {
        body.push(
            div()
                .pt_1()
                .text_xs()
                .text_color(p.faint_foreground)
                .child(format!(
                    "Compacts automatically at {}",
                    tokens(num(info, "autoCompactThreshold"))
                ))
                .into_any_element(),
        );
    }
    (summary, body)
}

// -- /usage --------------------------------------------------------------------

fn limit_label(limit: &Value) -> String {
    let kind = text(limit, "kind").unwrap_or("");
    let model = limit
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str);
    let span = match text(limit, "group").unwrap_or("") {
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

/// What a usage behaviour means, in words.
fn behavior_label(key: &str) -> String {
    match key {
        "long_context" => "ran above 150k context".into(),
        "subagent_heavy" => "came from subagent-heavy sessions".into(),
        "cron" => "came from sessions active for 8+ hours".into(),
        "high_parallel" => "ran while 4+ sessions were in parallel".into(),
        other => other.replace('_', " "),
    }
}

/// Kinds of usage sources, for the filter.
const SOURCES: [(&str, &str, &str); 4] = [
    ("agents", "Subagents", "Subagent"),
    ("skills", "Skills", "Skill"),
    ("plugins", "Plugins", "plugin"),
    ("mcp_servers", "MCP", "MCP"),
];

fn usage_body(
    info: &Value,
    card: &InfoCard,
    thread: &Entity<ThreadState>,
    ix: usize,
    p: &Palette,
) -> (String, Vec<AnyElement>) {
    let mut body = Vec::new();
    let plan = text(info, "subscription_type")
        .map(|s| s[..1].to_uppercase() + &s[1..])
        .unwrap_or_default();
    let limits = arr(info, "/rate_limits/limits");
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
        body.push(limit_row(
            limit_label(limit),
            text(limit, "resets_at").and_then(reset_time),
            num(limit, "percent") as f32,
            text(limit, "severity") != Some("normal"),
            p,
        ));
    }

    // This session, at a glance.
    if let Some(session) = info.get("session") {
        let models: Vec<(&String, &Value)> = session
            .get("model_usage")
            .and_then(Value::as_object)
            .map(|m| m.iter().collect())
            .unwrap_or_default();
        let cost = num(session, "total_cost_usd");
        let searches: f64 = models
            .iter()
            .map(|(_, u)| num(u, "webSearchRequests"))
            .sum();
        let mut cells = vec![
            format!("Cost ${cost:.2}"),
            format!(
                "Active {}",
                crate::items::human_duration(num(session, "total_api_duration_ms") as i64)
            ),
            format!("Web searches {}", searches as i64),
        ];
        let model_cost: f64 = models.iter().map(|(_, u)| num(u, "costUSD")).sum();
        if model_cost > 0. {
            for (id, u) in &models {
                cells.push(format!(
                    "{} {}%",
                    model_name(id).split(' ').next().unwrap_or(""),
                    (num(u, "costUSD") / model_cost * 100.).round() as i64
                ));
            }
        }
        let (read, input, write): (f64, f64, f64) =
            models.iter().fold((0., 0., 0.), |a, (_, u)| {
                (
                    a.0 + num(u, "cacheReadInputTokens"),
                    a.1 + num(u, "inputTokens"),
                    a.2 + num(u, "cacheCreationInputTokens"),
                )
            });
        if read + input + write > 0. {
            cells.push(format!(
                "Cache hit {}%",
                (read / (read + input + write) * 100.).round() as i64
            ));
        }
        let (added, removed) = (
            num(session, "total_lines_added") as i64,
            num(session, "total_lines_removed") as i64,
        );
        if added + removed > 0 {
            cells.push(format!("Lines +{added} −{removed}"));
        }
        body.push(section_title("This session", p).into_any_element());
        body.push(grid(cells, p));

        // Breakdown for one model (click to switch).
        if !models.is_empty() {
            let pick = card.model.min(models.len() - 1);
            let (id, u) = models[pick];
            let n = models.len();
            body.push(
                h_flex()
                    .pt_3()
                    .pb_1()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Breakdown"),
                    )
                    .child(
                        chip(format!("card-{ix}-model"), model_name(id), false, p).when(
                            n > 1,
                            |d| {
                                d.child(Icon::new(IconName::ChevronDown).xsmall().ml_1())
                                    .flex()
                                    .items_center()
                                    .on_click(on_card(thread, ix, move |c| {
                                        c.model = (c.model + 1) % n;
                                    }))
                            },
                        ),
                    )
                    .into_any_element(),
            );
            for (key, label) in [
                ("inputTokens", "Input"),
                ("outputTokens", "Output"),
                ("cacheReadInputTokens", "Cache read"),
                ("cacheCreationInputTokens", "Cache write"),
                ("thinkingTokens", "Thinking"),
            ] {
                if u.get(key).is_some() && (key != "thinkingTokens" || num(u, key) > 0.) {
                    body.push(stat_row(label.into(), tokens(num(u, key)), None, p));
                }
            }
            body.push(stat_row(
                "Cost".into(),
                format!("${:.2}", num(u, "costUSD")),
                None,
                p,
            ));
        }
    }

    // What's using the limits: last day or week, by kind of source.
    let range = if card.week { "week" } else { "day" };
    if let Some(b) = info.pointer(&format!("/behaviors/{range}")) {
        let kinds: Vec<usize> = SOURCES
            .iter()
            .enumerate()
            .filter(|(_, (key, ..))| !arr(b, &format!("/{key}")).is_empty())
            .map(|(i, _)| i + 1)
            .collect();
        let filter = if kinds.contains(&card.filter) {
            card.filter
        } else {
            0
        };
        let mut head = h_flex()
            .pt_3()
            .pb_1()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("What's using your limits?"),
            )
            .child(
                chip(format!("card-{ix}-day"), "24h", !card.week, p).on_click(on_card(
                    thread,
                    ix,
                    |c| c.week = false,
                )),
            )
            .child(
                chip(format!("card-{ix}-week"), "7d", card.week, p).on_click(on_card(
                    thread,
                    ix,
                    |c| c.week = true,
                )),
            )
            .child(div().w_px().h(px(14.)).mx_1().bg(p.border))
            .child(
                chip(format!("card-{ix}-all"), "All", filter == 0, p).on_click(on_card(
                    thread,
                    ix,
                    |c| c.filter = 0,
                )),
            );
        for k in &kinds {
            let k = *k;
            head = head.child(
                chip(
                    format!("card-{ix}-kind-{k}"),
                    SOURCES[k - 1].1,
                    filter == k,
                    p,
                )
                .on_click(on_card(thread, ix, move |c| c.filter = k)),
            );
        }
        body.push(head.into_any_element());
        body.push(
            div()
                .text_xs()
                .text_color(p.faint_foreground)
                .child(format!(
                    "Last {} · {} requests in {} sessions · approximate, overlapping measures · this Mac only",
                    if card.week { "7 days" } else { "24h" },
                    num(b, "request_count") as i64,
                    num(b, "session_count") as i64
                ))
                .into_any_element(),
        );
        if filter == 0 {
            let tips = arr(b, "/recommendations");
            for beh in arr(b, "/behaviors") {
                let key = text(beh, "key").unwrap_or("");
                let tip = tips.iter().find(|t| text(t, "key") == Some(key)).map(|t| {
                    let mut s = text(t, "title").unwrap_or("").to_owned();
                    if let Some(d) = text(t, "detail") {
                        s.push_str(&format!(" ({d})"));
                    }
                    s
                });
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
                                .child(format!("{}%", num(beh, "pct") as i64)),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_color(p.foreground).child(behavior_label(key)))
                                .children(tip.map(|t| {
                                    div().text_xs().text_color(p.faint_foreground).child(t)
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        // The sources themselves.
        let mut sources: Vec<(String, f32)> = Vec::new();
        for (i, (key, _, noun)) in SOURCES.iter().enumerate() {
            if filter != 0 && filter != i + 1 {
                continue;
            }
            for s in arr(b, &format!("/{key}")) {
                sources.push((
                    format!("{} {noun}", text(s, "name").unwrap_or("?")),
                    num(s, "pct") as f32,
                ));
            }
        }
        sources.sort_by(|a, b| b.1.total_cmp(&a.1));
        if !sources.is_empty() {
            let mut list = v_flex().pt_2().gap_1();
            for (name, pct) in sources.into_iter().take(10) {
                list = list.child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .text_sm()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(p.muted_foreground)
                                .child(name),
                        )
                        .child(div().w(px(140.)).child(meter(pct / 100., p.accent, p)))
                        .child(
                            div()
                                .w(px(40.))
                                .flex()
                                .justify_end()
                                .child(format!("{}%", pct as i64)),
                        ),
                );
            }
            body.push(list.into_any_element());
        } else if filter != 0 {
            body.push(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child("Nothing attributed here yet.")
                    .into_any_element(),
            );
        }
        // The top tip.
        if let Some(t) = arr(b, "/recommendations").first() {
            body.push(section_title("Tip", p).into_any_element());
            body.push(
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(format!(
                        "{}{}",
                        text(t, "title").unwrap_or(""),
                        text(t, "detail")
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    ))
                    .into_any_element(),
            );
        }
    }
    (plan, body)
}

// -- Codex /status -------------------------------------------------------------

fn window_label(mins: f64) -> String {
    match mins as i64 {
        300 => "5-hour limit".into(),
        10080 => "Weekly limit".into(),
        m if m % 1440 == 0 => format!("{}-day limit", m / 1440),
        m if m % 60 == 0 => format!("{}-hour limit", m / 60),
        m => format!("{m}-minute limit"),
    }
}

fn status_body(info: &Value, p: &Palette) -> (String, Vec<AnyElement>) {
    let mut body = Vec::new();
    let account = info.pointer("/account/account").unwrap_or(&Value::Null);
    let plan = text(account, "planType")
        .map(|s| s[..1].to_uppercase() + &s[1..])
        .unwrap_or_default();
    // Who and what.
    let session = info.get("session").unwrap_or(&Value::Null);
    let mut rows = Vec::new();
    if let Some(email) = text(account, "email") {
        rows.push(("Account", email.to_owned()));
    } else if let Some(kind) = text(account, "type") {
        rows.push(("Signed in with", kind.to_owned()));
    }
    if let Some(m) = text(session, "model") {
        let effort = text(session, "effort")
            .map(|e| format!(" · {e}"))
            .unwrap_or_default();
        rows.push(("Model", format!("{m}{effort}")));
    }
    if let Some(cwd) = text(session, "cwd") {
        rows.push(("Folder", crate::workspace::tilde_path_str(cwd)));
    }
    if let Some(t) = text(session, "threadId") {
        rows.push(("Session", t.to_owned()));
    }
    for (k, v) in rows {
        body.push(stat_row(k.into(), v, None, p));
    }
    // This thread's tokens and context.
    if let Some(u) = info.get("tokenUsage").filter(|u| !u.is_null()) {
        let total = u.get("total").unwrap_or(&Value::Null);
        let last = u.get("last").unwrap_or(&Value::Null);
        body.push(section_title("This thread", p).into_any_element());
        body.push(grid(
            vec![
                format!("Input {}", tokens(num(total, "inputTokens"))),
                format!("Cached {}", tokens(num(total, "cachedInputTokens"))),
                format!("Output {}", tokens(num(total, "outputTokens"))),
                format!("Reasoning {}", tokens(num(total, "reasoningOutputTokens"))),
                format!("Total {}", tokens(num(total, "totalTokens"))),
            ],
            p,
        ));
        let window = num(u, "modelContextWindow");
        if window > 0. {
            let used = num(last, "totalTokens").max(num(last, "inputTokens"));
            let left = ((1. - used / window) * 100.).clamp(0., 100.);
            body.push(
                v_flex()
                    .pt_2()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .text_sm()
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(p.muted_foreground)
                                    .child("Context window"),
                            )
                            .child(format!(
                                "{} / {} · {}% left",
                                tokens(used),
                                tokens(window),
                                left.round() as i64
                            )),
                    )
                    .child(meter((used / window) as f32, p.accent, p))
                    .into_any_element(),
            );
        }
    }
    // Limits.
    let limits = info
        .pointer("/rateLimits/rateLimits")
        .unwrap_or(&Value::Null);
    let windows: Vec<&Value> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| limits.get(*k).filter(|w| !w.is_null()))
        .collect();
    if !windows.is_empty() {
        body.push(section_title("Limits", p).into_any_element());
        for w in windows {
            body.push(limit_row(
                window_label(num(w, "windowDurationMins")),
                w.get("resetsAt")
                    .and_then(Value::as_i64)
                    .and_then(reset_unix),
                num(w, "usedPercent") as f32,
                num(w, "usedPercent") >= 80.,
                p,
            ));
        }
        if let Some(c) = limits.get("credits").filter(|c| {
            c.get("hasCredits").and_then(Value::as_bool) == Some(true)
                || c.get("unlimited").and_then(Value::as_bool) == Some(true)
        }) {
            let v = if c.get("unlimited").and_then(Value::as_bool) == Some(true) {
                "Unlimited".to_owned()
            } else {
                text(c, "balance").unwrap_or("0").to_owned()
            };
            body.push(stat_row("Credits".into(), v, None, p));
        }
    }
    // Usage history.
    if let Some(usage) = info.get("usage").filter(|u| !u.is_null()) {
        let s = usage.get("summary").unwrap_or(&Value::Null);
        body.push(section_title("Your Codex usage", p).into_any_element());
        body.push(grid(
            vec![
                format!("Lifetime {}", tokens(num(s, "lifetimeTokens"))),
                format!("Best day {}", tokens(num(s, "peakDailyTokens"))),
                format!("Streak {} days", num(s, "currentStreakDays") as i64),
            ],
            p,
        ));
        // The last two weeks, a bar per day.
        let days = arr(usage, "/dailyUsageBuckets");
        let recent: Vec<&Value> = days.iter().rev().take(14).rev().copied().collect();
        let peak = recent.iter().map(|d| num(d, "tokens")).fold(1., f64::max);
        if !recent.is_empty() {
            let mut bars = h_flex().pt_2().h(px(52.)).gap(px(3.)).items_end();
            for d in &recent {
                let share = (num(d, "tokens") / peak) as f32;
                bars = bars.child(
                    div()
                        .flex_1()
                        .h(relative(share.max(0.03)))
                        .rounded(px(2.))
                        .bg(p.accent.opacity(0.75)),
                );
            }
            body.push(bars.into_any_element());
            body.push(
                h_flex()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child(
                        div()
                            .flex_1()
                            .child(text(recent[0], "startDate").unwrap_or("").to_owned()),
                    )
                    .child(
                        text(recent[recent.len() - 1], "startDate")
                            .unwrap_or("")
                            .to_owned(),
                    )
                    .into_any_element(),
            );
        }
    }
    (plan, body)
}

/// The card as plain text (Copy).
fn card_text(kind: AgentInfoKind, info: &Value) -> String {
    let mut out = String::new();
    match kind {
        AgentInfoKind::Context => {
            out.push_str(&format!(
                "Context window: {} / {} ({}%)\n",
                tokens(num(info, "totalTokens")),
                tokens(num(info, "maxTokens")),
                num(info, "percentage") as i64
            ));
            for c in arr(info, "/categories") {
                out.push_str(&format!(
                    "  {}: {}\n",
                    text(c, "name").unwrap_or(""),
                    tokens(num(c, "tokens"))
                ));
            }
        }
        AgentInfoKind::Usage => {
            for l in arr(info, "/rate_limits/limits") {
                out.push_str(&format!(
                    "{}: {}%\n",
                    limit_label(l),
                    num(l, "percent") as i64
                ));
            }
            if let Some(s) = info.get("session") {
                out.push_str(&format!("Session cost: ${:.2}\n", num(s, "total_cost_usd")));
            }
        }
        AgentInfoKind::Status => {
            let limits = info
                .pointer("/rateLimits/rateLimits")
                .unwrap_or(&Value::Null);
            for k in ["primary", "secondary"] {
                if let Some(w) = limits.get(k).filter(|w| !w.is_null()) {
                    out.push_str(&format!(
                        "{}: {}% used\n",
                        window_label(num(w, "windowDurationMins")),
                        num(w, "usedPercent") as i64
                    ));
                }
            }
        }
    }
    out
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
    let Some(card) = state.cards.get(ix) else {
        return div().into_any_element();
    };
    let kind = card.kind;
    let (title, command, icon) = match kind {
        AgentInfoKind::Context => ("Context window", "/context", IconName::Gauge),
        AgentInfoKind::Usage => ("Usage", "/usage", IconName::ChartPie),
        AgentInfoKind::Status => ("Status", "/status", IconName::ChartPie),
    };
    let (summary, body): (String, Vec<AnyElement>) = match &card.state {
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
            AgentInfoKind::Context => context_body(info, card, thread, ix, p),
            AgentInfoKind::Usage => usage_body(info, card, thread, ix, p),
            AgentInfoKind::Status => status_body(info, p),
        },
    };
    let ready = match &card.state {
        CardState::Ready(info) => Some(info.clone()),
        _ => None,
    };
    let foldable = kind == AgentInfoKind::Context && ready.is_some();
    let open = card.open;
    let thread_id = state.id.clone();
    let (t_fold, t_reload, t_close) = (thread.clone(), thread.clone(), thread.clone());
    let store_reload = store.clone();
    let compact = (kind == AgentInfoKind::Usage).then(|| {
        let (store, id) = (store.clone(), thread_id.clone());
        h_flex().pt_3().gap_2().child(
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
                .gap_1()
                .px_4()
                .py_3()
                .rounded(px(12.))
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .child(
                    h_flex()
                        .id(SharedString::from(format!("card-head-{ix}")))
                        .pb_1()
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
                                Icon::new(if open {
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
                        .when_some(ready, |d, info| {
                            d.child({
                                let id = format!("card-copy-{ix}");
                                let copied = crate::items::just_copied(&id);
                                icon_button(
                                    id.clone(),
                                    if copied {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    },
                                    if copied { "Copied" } else { "Copy" },
                                    p,
                                )
                                .when(copied, |d| d.text_color(p.success))
                                .claims_press()
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        card_text(kind, &info),
                                    ));
                                    crate::items::mark_copied(id.clone(), window, cx);
                                })
                            })
                        })
                        .child(
                            icon_button(
                                format!("card-reload-{ix}"),
                                IconName::RotateCw,
                                "Refresh",
                                p,
                            )
                            .claims_press()
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                let client = store_reload.read(cx).client();
                                t_reload.update(cx, |t, cx| t.load_card(ix, client, cx));
                            }),
                        )
                        .child(
                            icon_button(format!("card-close-{ix}"), IconName::X, "Dismiss", p)
                                .claims_press()
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

/// An agent's plan and its limits, from its `/usage` report (Claude Code)
/// or its `/status` report (Codex): the plan's name, then a row per limit.
pub(crate) fn plan_limits(info: &Value, p: &Palette) -> (Option<String>, Vec<AnyElement>) {
    let capital = |s: &str| s[..1].to_uppercase() + &s[1..];
    let mut rows = Vec::new();
    // Claude Code.
    let claude = arr(info, "/rate_limits/limits");
    for limit in &claude {
        rows.push(limit_row(
            limit_label(limit),
            text(limit, "resets_at").and_then(reset_time),
            num(limit, "percent") as f32,
            text(limit, "severity") != Some("normal"),
            p,
        ));
    }
    // Codex.
    let limits = info
        .pointer("/rateLimits/rateLimits")
        .unwrap_or(&Value::Null);
    for w in ["primary", "secondary"]
        .iter()
        .filter_map(|k| limits.get(*k).filter(|w| !w.is_null()))
    {
        rows.push(limit_row(
            window_label(num(w, "windowDurationMins")),
            w.get("resetsAt")
                .and_then(Value::as_i64)
                .and_then(reset_unix),
            num(w, "usedPercent") as f32,
            num(w, "usedPercent") >= 80.,
            p,
        ));
    }
    if let Some(c) = limits.get("credits").filter(|c| {
        c.get("hasCredits").and_then(Value::as_bool) == Some(true)
            || c.get("unlimited").and_then(Value::as_bool) == Some(true)
    }) {
        let v = if c.get("unlimited").and_then(Value::as_bool) == Some(true) {
            "Unlimited".to_owned()
        } else {
            text(c, "balance").unwrap_or("0").to_owned()
        };
        rows.push(stat_row("Credits".into(), v, None, p));
    }
    let plan = text(info, "subscription_type")
        .or_else(|| {
            info.pointer("/account/account/planType")
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty())
        .map(capital);
    (plan, rows)
}

/// The most used of an agent's plan limits, in percent (the same reports
/// as [`plan_limits`]).
pub(crate) fn peak_percent(info: &Value) -> Option<f32> {
    let claude = arr(info, "/rate_limits/limits")
        .into_iter()
        .map(|l| num(l, "percent"));
    let codex = ["primary", "secondary"].into_iter().filter_map(|k| {
        info.pointer(&format!("/rateLimits/rateLimits/{k}/usedPercent"))
            .and_then(Value::as_f64)
    });
    claude.chain(codex).reduce(f64::max).map(|v| v as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_percent_is_the_busiest_limit() {
        let claude: Value = serde_json::json!({"rate_limits": {"limits": [
            {"percent": 12.0}, {"percent": 61.5}]}});
        assert_eq!(peak_percent(&claude), Some(61.5));
        let codex: Value = serde_json::json!({"rateLimits": {"rateLimits": {
            "primary": {"usedPercent": 40.0}, "secondary": null}}});
        assert_eq!(peak_percent(&codex), Some(40.0));
        assert_eq!(peak_percent(&serde_json::json!({})), None);
    }

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(178_700.), "178.7k");
        assert_eq!(tokens(1_000_000.), "1M");
        assert_eq!(tokens(33_000.), "33k");
        assert_eq!(tokens(32.), "32");
    }

    #[test]
    fn labels() {
        let all: Value = serde_json::json!({"kind": "weekly_all", "group": "weekly"});
        assert_eq!(limit_label(&all), "Weekly · all models");
        let fable: Value = serde_json::json!({"kind": "weekly_scoped", "group": "weekly",
            "scope": {"model": {"display_name": "Fable"}}});
        assert_eq!(limit_label(&fable), "Weekly · Fable");
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(window_label(300.), "5-hour limit");
        assert_eq!(window_label(10080.), "Weekly limit");
        assert_eq!(words("syncedSkills"), "synced skills");
        assert_eq!(
            tool_name("mcp__agentloft__browser_click"),
            "agentloft · browser_click"
        );
        assert_eq!(tool_name("Bash"), "Bash");
    }
}
