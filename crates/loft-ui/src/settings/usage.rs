//! Settings ▸ Usage: what the agents have used across every thread, in
//! tokens or cost (by day, agent, model, project and thread, from the
//! daemon's `UsageReport`), and each agent's plan limits (asked of the agent itself,
//! through the most recent thread on it, as `/usage` and `/status` do).

use super::{SettingsView, web::heading};
use crate::theme::Palette;
use chrono::Datelike as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::date_picker::{
    DatePicker, DatePickerEvent, DatePickerState, DateRangePreset, DateTime,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px, relative,
};
use loft_protocol::{AgentInfoKind, HarnessId, Request, Response, ThreadStatus};
use serde_json::Value;
use std::collections::HashMap;

/// The agents whose plan limits can be asked for, and how.
const LIMIT_AGENTS: [(&str, &str, AgentInfoKind); 2] = [
    (HarnessId::CLAUDE, "Claude Code", AgentInfoKind::Usage),
    (HarnessId::CODEX, "Codex", AgentInfoKind::Status),
];

#[derive(Default)]
pub(super) struct UsagePage {
    report: Option<Value>,
    loading: bool,
    /// The local days shown (days since 1970-01-01); `None`: since the
    /// first turn / up to today. The page opens on the last 30 days.
    from: Option<i64>,
    to: Option<i64>,
    picker: Option<Entity<DatePickerState>>,
    _picker_sub: Option<Subscription>,
    /// What the numbers count, and what the table is broken down by.
    metric: Metric,
    by: By,
    /// Per agent: `None` asking, `Some(Ok)` its report, `Some(Err)` why not.
    limits: HashMap<&'static str, Option<Result<Value, String>>>,
}

enum Limits<'a> {
    NoThread,
    Asking,
    Ready(&'a Value),
    Failed(&'a str),
}

impl SettingsView {
    /// Ask the daemon for the report and the agents for their limits.
    pub(super) fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        self.usage.loading = true;
        let offset = chrono::Local::now().offset().local_minus_utc() / 60;
        let report = client.request(Request::UsageReport {
            utc_offset_min: offset,
            from_day: self.usage.from,
            to_day: self.usage.to,
        });
        cx.spawn(async move |this, cx| {
            let result = report.await;
            let _ = this.update(cx, |this, cx| {
                this.usage.loading = false;
                match result {
                    Ok(Response::Json { value }) => this.usage.report = Some(value),
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.message),
                }
                cx.notify();
            });
        })
        .detach();
        // Limits: the agent's own report, through its most recent thread.
        for (agent, _, kind) in LIMIT_AGENTS {
            let Some(thread_id) = self.latest_thread(agent, cx) else {
                self.usage.limits.remove(agent);
                continue;
            };
            self.usage.limits.insert(agent, None);
            let ask = client.request(Request::AgentInfo { thread_id, kind });
            cx.spawn(async move |this, cx| {
                let result = ask.await;
                let _ = this.update(cx, |this, cx| {
                    let got = match result {
                        Ok(Response::AgentInfo { info }) => Ok(info),
                        Ok(_) => Err("The agent gave no report.".to_owned()),
                        Err(err) => Err(err.message),
                    };
                    this.usage.limits.insert(agent, Some(got));
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// The most recently used thread on `agent` that isn't archived.
    fn latest_thread(&self, agent: &str, cx: &Context<Self>) -> Option<loft_protocol::ThreadId> {
        self.store
            .read(cx)
            .workspace
            .threads
            .iter()
            .find(|t| t.harness.as_str() == agent && t.status != ThreadStatus::Archived)
            .map(|t| t.id.clone())
    }

    /// The range picker, made once: the last 30 days to start with.
    fn usage_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DatePickerState> {
        if let Some(picker) = &self.usage.picker {
            return picker.clone();
        }
        let today = chrono::Local::now().date_naive();
        let start = today - chrono::Days::new(29);
        self.usage.from = Some(epoch_day(start));
        self.usage.to = Some(epoch_day(today));
        let picker = cx.new(|cx| {
            let mut state = DatePickerState::range(window, cx)
                .date_format("%b %-d, %Y")
                .number_of_months(2);
            state.set_date((start, today), window, cx);
            state
        });
        let sub = cx.subscribe(&picker, |this, _, event: &DatePickerEvent, cx| {
            let DatePickerEvent::Change(value) = event;
            let (from, to) = match value {
                DateTime::Range(start, end) => (start.map(|d| d.date()), end.map(|d| d.date())),
                DateTime::Single(day) => (day.map(|d| d.date()), day.map(|d| d.date())),
            };
            // Half a range: wait for its other end.
            if from.is_some() != to.is_some() {
                return;
            }
            this.usage.from = from.map(epoch_day);
            this.usage.to = to.map(epoch_day);
            this.refresh_usage(cx);
        });
        self.usage.picker = Some(picker.clone());
        self.usage._picker_sub = Some(sub);
        picker
    }

    pub(super) fn usage_page(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let picker = self.usage_picker(window, cx);
        if self.usage.report.is_none() && !self.usage.loading {
            self.refresh_usage(cx);
        }
        let metric = self.usage.metric;
        let mut page = v_flex().gap_1().child(self.usage_toolbar(&picker, p, cx));
        match self.usage.report.clone() {
            None => {
                page = page.child(
                    h_flex()
                        .gap_2()
                        .py_6()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child(Spinner::new().xsmall().color(p.muted_foreground))
                        .child("Adding up your threads…"),
                );
            }
            Some(r) => {
                let range = range_words(&r);
                page = page
                    .child(hero(&r, metric, &range, p))
                    .child(day_chart(&r, metric, p))
                    .child(heading("Totals", "", p))
                    .child(totals(&r["range"], p))
                    .child(heading(
                        match metric {
                            Metric::Tokens => "Tokens by type",
                            Metric::Cost => "Cost by type",
                        },
                        match metric {
                            Metric::Tokens => "",
                            Metric::Cost => "At API prices for each turn's model.",
                        },
                        p,
                    ))
                    .child(by_type(&r["range"], metric, p))
                    .child(self.breakdown(&r, metric, p, cx));
            }
        }
        page = page.child(heading(
            "Plan limits",
            "Asked of each agent through your most recent thread on it (this starts the agent if it isn't running; it uses no tokens).",
            p,
        ));
        for (agent, name, _) in LIMIT_AGENTS {
            let state = match self.usage.limits.get(agent) {
                None => Limits::NoThread,
                Some(None) => Limits::Asking,
                Some(Some(Ok(v))) => Limits::Ready(v),
                Some(Some(Err(e))) => Limits::Failed(e),
            };
            page = page.child(limits_card(agent, name, state, p));
        }
        page.pb_8().into_any_element()
    }

    fn usage_toolbar(
        &self,
        picker: &Entity<DatePickerState>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let today = chrono::Local::now().date_naive();
        let days = |n: u64| today - chrono::Days::new(n);
        let month_start = today.with_day(1).unwrap_or(today);
        let last_month_end = month_start - chrono::Days::new(1);
        let last_month_start = last_month_end.with_day(1).unwrap_or(last_month_end);
        let year_start = today.with_ordinal(1).unwrap_or(today);
        let presets = vec![
            DateRangePreset::range("Today", today, today),
            DateRangePreset::range("Yesterday", days(1), days(1)),
            DateRangePreset::range("Last 7 days", days(6), today),
            DateRangePreset::range("Last 30 days", days(29), today),
            DateRangePreset::range("Last 90 days", days(89), today),
            DateRangePreset::range("This month", month_start, today),
            DateRangePreset::range("Last month", last_month_start, last_month_end),
            DateRangePreset::range("This year", year_start, today),
            DateRangePreset::date_time("All time", DateTime::Range(None, None)),
        ];
        let metric = self.usage.metric;
        h_flex()
            .gap_2()
            .items_center()
            .pb_2()
            .child(segmented(
                "usage-metric",
                &[
                    ("Tokens", metric == Metric::Tokens),
                    ("Cost", metric == Metric::Cost),
                ],
                p,
                cx,
                |this, ix, cx| {
                    this.usage.metric = if ix == 0 {
                        Metric::Tokens
                    } else {
                        Metric::Cost
                    };
                    cx.notify();
                },
            ))
            .child(div().flex_1())
            .child(
                div().w(px(250.)).child(
                    DatePicker::new(picker)
                        .small()
                        .placeholder("All time")
                        .presets(presets),
                ),
            )
            .child(
                Button::new("usage-refresh")
                    .cursor_pointer()
                    .ghost()
                    .xsmall()
                    .icon(IconName::RotateCcw)
                    .tooltip("Refresh")
                    .loading(self.usage.loading)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_usage(cx))),
            )
            .into_any_element()
    }

    /// The breakdown table and its tabs.
    fn breakdown(
        &self,
        r: &Value,
        metric: Metric,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let by = self.usage.by;
        let tabs = segmented(
            "usage-by",
            &BY.map(|b| (b.label(), b == by)),
            p,
            cx,
            |this, ix, cx| {
                this.usage.by = BY[ix];
                cx.notify();
            },
        );
        v_flex()
            .child(
                h_flex()
                    .items_end()
                    .gap_3()
                    .child(div().flex_1().child(heading("Breakdown", "", p)))
                    .child(tabs),
            )
            .child(table(r, by, metric, p))
            .into_any_element()
    }
}

/// A row of choices, one picked; `pick` gets the index clicked.
fn segmented(
    id: &'static str,
    options: &[(&'static str, bool)],
    p: &Palette,
    cx: &mut Context<SettingsView>,
    pick: impl Fn(&mut SettingsView, usize, &mut Context<SettingsView>) + Clone + 'static,
) -> AnyElement {
    let mut row = h_flex().gap(px(2.)).p(px(2.)).rounded(px(8.)).bg(p.muted);
    for (ix, (label, active)) in options.iter().copied().enumerate() {
        let pick = pick.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-{ix}")))
                .px_2p5()
                .py(px(3.))
                .rounded(px(6.))
                .text_xs()
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(p.elevated).text_color(p.foreground).shadow_sm()
                })
                .when(!active, |d| {
                    d.text_color(p.muted_foreground)
                        .hover(|d| d.text_color(p.foreground))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| pick(this, ix, cx))),
        );
    }
    row.into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Metric {
    #[default]
    Tokens,
    Cost,
}

impl Metric {
    fn of(self, v: &Value) -> f64 {
        match self {
            Metric::Tokens => n(v, "tokens") as f64,
            Metric::Cost => cost(v).unwrap_or(0.),
        }
    }

    fn show(self, x: f64) -> String {
        match self {
            Metric::Tokens => tokens(x as u64),
            Metric::Cost => dollars(x),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum By {
    #[default]
    Model,
    Agent,
    Project,
    Thread,
    Day,
}

const BY: [By; 5] = [By::Model, By::Agent, By::Project, By::Thread, By::Day];

impl By {
    fn label(self) -> &'static str {
        match self {
            By::Model => "Model",
            By::Agent => "Agent",
            By::Project => "Project",
            By::Thread => "Thread",
            By::Day => "Day",
        }
    }
}

// -- numbers -----------------------------------------------------------------

/// Days since 1970-01-01.
fn epoch_day(date: chrono::NaiveDate) -> i64 {
    (date - chrono::NaiveDate::default()).num_days()
}

/// "Oct 13 – today", "today", "Nov 3".
fn range_words(r: &Value) -> String {
    let (Some(from), Some(to)) = (r["from_day"].as_i64(), r["to_day"].as_i64()) else {
        return "all time".into();
    };
    let today = r["today"].as_i64().unwrap_or(to);
    match (from, to) {
        (f, t) if f == t && t == today => "today".into(),
        (f, t) if f == t => day_label(f, false),
        (f, t) if t == today => format!("{} – today", day_label(f, false)),
        (f, t) => format!("{} – {}", day_label(f, false), day_label(t, false)),
    }
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn cost(v: &Value) -> Option<f64> {
    v.get("cost").and_then(Value::as_f64)
}

/// "1.2M", "34K", "980".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}K", n / 1_000),
        n if n >= 1_000 => format!("{:.1}K", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// "$1,234.56", "<$0.01".
fn dollars(c: f64) -> String {
    if c > 0. && c < 0.01 {
        return "<$0.01".into();
    }
    let cents = (c * 100.).round() as i64;
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("${grouped}.{:02}", cents % 100)
}

fn percent(share: f64) -> String {
    match share * 100. {
        x if x > 0. && x < 0.1 => "<0.1%".into(),
        x => format!("{x:.1}%"),
    }
}

/// Local day number (days since the epoch) → "Nov 11".
fn day_label(day: i64, long: bool) -> String {
    let date = chrono::DateTime::from_timestamp(day * 86_400, 0)
        .map(|d| d.date_naive())
        .unwrap_or_default();
    date.format(if long { "%a, %b %-d" } else { "%b %-d" })
        .to_string()
}

fn agent_name(id: &str) -> String {
    match id {
        HarnessId::CLAUDE => "Claude Code".into(),
        HarnessId::CODEX => "Codex".into(),
        HarnessId::OPENCODE => "OpenCode".into(),
        HarnessId::GEMINI => "Gemini CLI".into(),
        other => other.to_owned(),
    }
}

/// Each agent's colour, the same wherever it shows (never by rank).
fn agent_color(id: &str, p: &Palette) -> Hsla {
    let hex = |h: u32| Hsla::from(gpui_kit::rgb(h));
    match id {
        HarnessId::CLAUDE => hex(0xD97757),
        HarnessId::CODEX => p.foreground.opacity(0.85),
        HarnessId::OPENCODE => hex(0x5B9BD5),
        HarnessId::GEMINI => hex(0x4FAE8B),
        _ => p.muted_foreground,
    }
}

// -- pieces ------------------------------------------------------------------

/// The total in big, then each agent: its threads, share and the other
/// measure.
fn hero(r: &Value, metric: Metric, range: &str, p: &Palette) -> AnyElement {
    let all = &r["range"];
    let total = metric.of(all);
    let priced = cost(all).is_some();
    let big = match (metric, priced) {
        (Metric::Cost, false) => "—".to_owned(),
        _ => metric.show(total),
    };
    let mut note = format!(
        "{} · {} turns · {range}",
        match n(r, "threads") {
            1 => "1 thread".to_owned(),
            k => format!("{k} threads"),
        },
        n(all, "turns")
    );
    let unpriced = n(all, "unpriced");
    if metric == Metric::Cost && unpriced > 0 {
        note.push_str(&format!(
            " · {unpriced} {} without a known model left out",
            if unpriced == 1 { "turn" } else { "turns" }
        ));
    }
    let mut agents = v_flex().gap_3().pt_4();
    for a in r["agents"].as_array().into_iter().flatten() {
        let id = a["key"].as_str().unwrap_or_default();
        let share = if total > 0. { metric.of(a) / total } else { 0. };
        let other = match metric {
            Metric::Tokens => cost(a).map(dollars),
            Metric::Cost => Some(format!("{} tokens", tokens(n(a, "tokens")))),
        };
        let value = match (metric, cost(a)) {
            (Metric::Cost, None) => "—".to_owned(),
            _ => metric.show(metric.of(a)),
        };
        let what = match metric {
            Metric::Tokens => "tokens",
            Metric::Cost => "cost",
        };
        let sub = match (metric, cost(a)) {
            (Metric::Cost, None) => "No price known for its model".to_owned(),
            _ => std::iter::once(format!("{} of {what}", percent(share)))
                .chain(other)
                .collect::<Vec<_>>()
                .join(" · "),
        };
        agents = agents.child(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size(px(8.)).rounded_full().bg(agent_color(id, p)))
                        .child(crate::brand::agent_logo(id, px(14.), p))
                        .child(div().text_sm().child(agent_name(id)))
                        .child(div().text_xs().text_color(p.faint_foreground).child(
                            match n(a, "threads") {
                                1 => "1 thread".to_owned(),
                                k => format!("{k} threads"),
                            },
                        ))
                        .child(div().flex_1())
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(value)),
                )
                .child(
                    div()
                        .pl(px(16.))
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(sub),
                ),
        );
    }
    v_flex()
        .child(
            div()
                .text_3xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(big),
        )
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(note)
                .when(metric == Metric::Cost && priced, |d| {
                    d.child("· API estimate").child(
                        div()
                            .id("usage-estimate-info")
                            .child(Icon::new(IconName::Info).xsmall())
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(
                                    "What the tokens would cost at API list prices. Claude Code works this out itself for each turn; for the other agents it is each turn's tokens times its model's price (built in, or [pricing] in config.toml). A subscription bills differently.",
                                )
                                .build(window, cx)
                            }),
                    )
                }),
        )
        .child(agents)
        .into_any_element()
}

/// A round number just above `x` for the top of an axis: 1, 2 or 5 × 10ⁿ.
fn nice_ceiling(x: f64) -> f64 {
    if x <= 0. {
        return 1.;
    }
    let base = 10f64.powf(x.log10().floor());
    [1., 2., 5., 10.]
        .iter()
        .map(|m| m * base)
        .find(|v| *v >= x)
        .unwrap_or(10. * base)
}

/// Daily (or weekly) bars, each stacked by agent, on a light grid.
fn day_chart(r: &Value, metric: Metric, p: &Palette) -> AnyElement {
    let days = r["days"].as_array().cloned().unwrap_or_default();
    let weekly = days.first().and_then(|d| d["span"].as_i64()) == Some(7);
    let title = match (metric, weekly) {
        (Metric::Tokens, false) => "Daily processed tokens",
        (Metric::Tokens, true) => "Weekly processed tokens",
        (Metric::Cost, false) => "Daily cost",
        (Metric::Cost, true) => "Weekly cost",
    };
    // Agents bottom to top, in the order of the list above.
    let order: Vec<String> = r["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["key"].as_str().map(str::to_owned))
        .collect();
    let peak = days.iter().map(|d| metric.of(d)).fold(0., f64::max);
    let top = nice_ceiling(peak);
    let height = 150.;
    let today = r["today"].as_i64().unwrap_or(0);
    let mut bars = h_flex().size_full().gap(px(2.)).items_end();
    for (ix, d) in days.iter().enumerate() {
        let day = d["day"].as_i64().unwrap_or(0);
        let when = if weekly {
            format!("Week of {}", day_label(day, false))
        } else {
            day_label(day, true)
        };
        let mut tip = format!("{when}\n{} tokens", tokens(n(d, "tokens")));
        if let Some(c) = cost(d) {
            tip.push_str(&format!(" · {}", dollars(c)));
        }
        tip.push_str(&format!(" · {} turns", n(d, "turns")));
        for id in &order {
            let a = &d["agents"][id];
            if n(a, "turns") > 0 {
                tip.push_str(&format!(
                    "\n{}: {}",
                    agent_name(id),
                    metric.show(metric.of(a))
                ));
            }
        }
        let mut stack = v_flex().w_full().flex_col_reverse();
        for id in &order {
            let v = metric.of(&d["agents"][id]);
            if v > 0. {
                stack = stack.child(
                    div()
                        .w_full()
                        .h(px(((v / top) as f32 * height).max(1.)))
                        .bg(agent_color(id, p)),
                );
            }
        }
        let span = d["span"].as_i64().unwrap_or(1);
        let current = (day..day + span).contains(&today);
        bars = bars.child(
            div()
                .id(SharedString::from(format!("usage-day-{ix}")))
                .flex_1()
                .h_full()
                .flex()
                .items_end()
                // The whole column takes the hover, not just the bar.
                .hover(|d| d.bg(p.hover))
                .rounded_t(px(4.))
                .child(
                    stack
                        .rounded_t(px(4.))
                        .overflow_hidden()
                        .when(!current, |d| d.opacity(0.85)),
                )
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                }),
        );
    }
    // Grid lines at 0, ½ and the top, labelled.
    let grid = v_flex()
        .absolute()
        .inset_0()
        .justify_between()
        .children([top, top / 2.].map(|v| {
            h_flex()
                .gap_2()
                .items_center()
                .child(div().h(px(1.)).flex_1().bg(p.border.opacity(0.5)))
                .child(
                    div()
                        .w(px(52.))
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(metric.show(v)),
                )
        }))
        .child(div().h(px(1.)));
    let first = days.first().and_then(|d| d["day"].as_i64());
    let last = days.last().and_then(|d| d["day"].as_i64());
    let to = r["to_day"].as_i64();
    v_flex()
        .gap_1()
        .pt_6()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div().relative().h(px(height)).mt_2().child(grid).child(
                div()
                    .absolute()
                    .inset_0()
                    .mr(px(60.))
                    .border_b_1()
                    .border_color(p.border)
                    .child(bars),
            ),
        )
        .child(
            h_flex()
                .mr(px(60.))
                .text_xs()
                .text_color(p.faint_foreground)
                .child(div().flex_1().children(first.map(|d| day_label(d, false))))
                .children(last.map(|d| match to {
                    Some(to) if to == today && d + 7 > to => "Today".to_owned(),
                    _ => day_label(d, false),
                })),
        )
        .into_any_element()
}

/// Processed, cached, uncached, written to cache, output; and the cost.
fn totals(range: &Value, p: &Palette) -> AnyElement {
    let cell = |label: &str, value: String| {
        v_flex()
            .w(relative(0.3))
            .gap_0p5()
            .whitespace_nowrap()
            .child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(label.to_owned()),
            )
            .child(div().text_lg().child(value))
    };
    h_flex()
        .flex_wrap()
        .gap_x_4()
        .gap_y_3()
        .pt_1()
        .child(cell("Processed tokens", tokens(n(range, "tokens"))))
        .child(cell("Cached input", tokens(n(range, "cache_read"))))
        .child(cell("Uncached input", tokens(n(range, "input"))))
        .child(cell("Output", tokens(n(range, "output"))))
        .child(cell(
            "Cost",
            cost(range).map(dollars).unwrap_or_else(|| "—".into()),
        ))
        .child(cell(
            "Cache savings",
            range["savings"]
                .as_f64()
                .filter(|s| *s > 0.)
                .map(dollars)
                .unwrap_or_else(|| "—".into()),
        ))
        .into_any_element()
}

/// One bar split by token type (2 px apart), with a legend: tokens, or
/// their cost at API prices.
fn by_type(range: &Value, metric: Metric, p: &Palette) -> AnyElement {
    let kinds = [
        ("Input", "input", p.accent.opacity(0.45)),
        ("Cache read", "cache_read", p.accent.opacity(0.75)),
        ("Cache write", "cache_write", p.info),
        ("Output", "output", p.foreground.opacity(0.85)),
    ];
    let value = |key: &str| match metric {
        Metric::Tokens => n(range, key) as f64,
        Metric::Cost => range["cost_by_kind"][key].as_f64().unwrap_or(0.),
    };
    let all = kinds.iter().map(|(_, k, _)| value(k)).sum::<f64>();
    if all <= 0. {
        return div()
            .py_2()
            .text_sm()
            .text_color(p.muted_foreground)
            .child("No known prices in this range.")
            .into_any_element();
    }
    let mut bar = h_flex().h(px(10.)).w_full().gap(px(2.));
    let mut legend = h_flex().flex_wrap().gap_x_4().gap_y_1().pt_2();
    for (label, key, color) in kinds {
        let v = value(key);
        if v > 0. {
            bar = bar.child(
                div()
                    .h_full()
                    .rounded(px(3.))
                    .flex_basis(px(0.))
                    .flex_grow((v / all) as f32)
                    .min_w(px(3.))
                    .bg(color),
            );
        }
        legend = legend.child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_xs()
                .child(div().size(px(8.)).rounded(px(2.)).bg(color))
                .child(div().text_color(p.muted_foreground).child(label))
                .child(metric.show(v))
                .child(div().text_color(p.faint_foreground).child(percent(v / all))),
        );
    }
    v_flex().pt_1().child(bar).child(legend).into_any_element()
}

/// Rows of # · name (its share underneath) · cost · share · tokens.
fn table(r: &Value, by: By, metric: Metric, p: &Palette) -> AnyElement {
    let mut rows: Vec<Value> = match by {
        By::Model => r["models"].as_array().cloned().unwrap_or_default(),
        By::Agent => r["agents"].as_array().cloned().unwrap_or_default(),
        By::Project => r["projects"].as_array().cloned().unwrap_or_default(),
        By::Thread => r["top_threads"].as_array().cloned().unwrap_or_default(),
        // Latest first, days that had any.
        By::Day => r["days"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| n(d, "turns") > 0)
            .rev()
            .cloned()
            .collect(),
    };
    if by != By::Day {
        rows.sort_by(|a, b| metric.of(b).total_cmp(&metric.of(a)));
    }
    if rows.is_empty() {
        return div()
            .py_2()
            .text_sm()
            .text_color(p.muted_foreground)
            .child("Nothing in this range.")
            .into_any_element();
    }
    let total = metric.of(&r["range"]).max(f64::EPSILON);
    let top = rows
        .iter()
        .map(|row| metric.of(row))
        .fold(0., f64::max)
        .max(f64::EPSILON);
    let num = |w: f32, text: String, color| {
        div()
            .w(px(w))
            .flex_none()
            .flex()
            .justify_end()
            .text_color(color)
            .child(text)
    };
    let mut list = v_flex().pt_1().child(
        h_flex()
            .gap_3()
            .pb_1p5()
            .border_b_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.muted_foreground)
            .child(div().w(px(20.)).child("#"))
            .child(div().flex_1().child(by.label()))
            .child(num(80., "Cost".into(), p.muted_foreground))
            .child(num(56., "Share".into(), p.muted_foreground))
            .child(num(64., "Tokens".into(), p.muted_foreground)),
    );
    for (ix, row) in rows.iter().enumerate() {
        let key = row["key"].as_str().unwrap_or_default();
        let (agent, name, sub) = match by {
            By::Agent => (Some(key.to_owned()), agent_name(key), None),
            By::Model => {
                let agent = row["agent"].as_str().unwrap_or_default();
                let model = row["model"].as_str().unwrap_or_default();
                let name = if model == "default" {
                    format!("{} default", agent_name(agent))
                } else {
                    model.to_owned()
                };
                (Some(agent.to_owned()), name, None)
            }
            By::Project => (
                None,
                row["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Removed project")
                    .to_owned(),
                None,
            ),
            By::Thread => (
                row["agent"].as_str().map(str::to_owned),
                row["title"].as_str().unwrap_or_default().to_owned(),
                row["project"].as_str().map(str::to_owned),
            ),
            By::Day => {
                let day = row["day"].as_i64().unwrap_or(0);
                let name = match row["span"].as_i64() {
                    Some(7) => format!("Week of {}", day_label(day, false)),
                    _ => day_label(day, true),
                };
                (None, name, None)
            }
        };
        let value = metric.of(row);
        let color = agent.as_deref().map_or(p.accent, |a| agent_color(a, p));
        list = list.child(
            h_flex()
                .gap_3()
                .items_center()
                .py_2()
                .border_b_1()
                .border_color(p.border.opacity(0.6))
                .text_sm()
                .child(
                    div()
                        .w(px(20.))
                        .text_color(p.faint_foreground)
                        .child((ix + 1).to_string()),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .min_w_0()
                                .children(
                                    agent
                                        .as_deref()
                                        .map(|a| crate::brand::agent_logo(a, px(13.), p)),
                                )
                                .child(div().min_w_0().truncate().child(name))
                                .children(sub.map(|s| {
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(p.faint_foreground)
                                        .child(s)
                                })),
                        )
                        // Its share, as a line under the name.
                        .child(
                            div().h(px(2.)).w_full().child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .w(relative((value / top) as f32 * 0.6))
                                    .bg(color),
                            ),
                        ),
                )
                .child(num(
                    80.,
                    cost(row).map(dollars).unwrap_or_else(|| "—".into()),
                    p.foreground,
                ))
                .child(num(56., percent(value / total), p.muted_foreground))
                .child(num(64., tokens(n(row, "tokens")), p.foreground)),
        );
    }
    list.into_any_element()
}

fn limits_card(agent: &str, name: &str, state: Limits<'_>, p: &Palette) -> AnyElement {
    let (plan, body): (Option<String>, Vec<AnyElement>) = match state {
        Limits::NoThread => (
            None,
            vec![
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(format!(
                        "No {name} thread yet: start one to see its limits."
                    ))
                    .into_any_element(),
            ],
        ),
        Limits::Asking => (
            None,
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
        Limits::Failed(why) => (
            None,
            vec![
                div()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(why.to_owned())
                    .into_any_element(),
            ],
        ),
        Limits::Ready(info) => {
            let (plan, rows) = crate::info_cards::plan_limits(info, p);
            let rows = if rows.is_empty() {
                vec![
                    div()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child(
                            "This sign-in has no plan limits to show (an API key pays per token).",
                        )
                        .into_any_element(),
                ]
            } else {
                rows
            };
            (plan, rows)
        }
    };
    v_flex()
        .mt_2()
        .p_3()
        .gap_1()
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .pb_1()
                .child(crate::brand::agent_logo(agent, px(14.), p))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(name.to_owned()),
                )
                .children(plan.map(|plan| {
                    div()
                        .px_1p5()
                        .rounded(px(5.))
                        .bg(p.accent_soft)
                        .text_xs()
                        .text_color(p.accent)
                        .child(plan)
                })),
        )
        .children(body)
        .into_any_element()
}
