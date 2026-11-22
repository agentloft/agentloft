//! The sidebar footer's usage button: each agent's plan limits in a card on
//! hover (asked directly of each installed agent without opening a thread
//! session, and kept for a minute), the busiest limit's percent beside the
//! icon once known, and a click opens Settings ▸ Usage.

use crate::spinner::Spinner;
use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{HarnessId, Request, Response, ThreadStatus};
use serde_json::Value;
use std::time::Duration;
use web_time::Instant;

/// The agents whose plan limits can be asked for.
pub(crate) const LIMIT_AGENTS: [(&str, &str); 3] = [
    (HarnessId::CLAUDE, "Claude Code"),
    (HarnessId::CODEX, "Codex"),
    (HarnessId::ANTIGRAVITY, "Antigravity"),
];

/// How long an answer is shown before hovering asks again.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// Whether `agent` is enabled and installed (or has an active thread if the
/// initial CLI probe hasn't answered yet).
pub(crate) fn limit_agent_available(store: &AppStore, agent: &str) -> bool {
    if store.disabled_agents.contains(agent) {
        return false;
    }
    if let Some(status) = store.harness_status.get(&HarnessId(agent.into())) {
        return status.installed;
    }
    store
        .workspace
        .threads
        .iter()
        .any(|t| t.harness.as_str() == agent && t.status != ThreadStatus::Archived)
}

pub(crate) struct PlanLimits {
    store: Entity<AppStore>,
    /// Per available agent: `None` asking, `Some(Ok)` its report,
    /// `Some(Err)` why not.
    limits: Vec<(&'static str, Option<Result<Value, String>>)>,
    asked_at: Option<Instant>,
}

impl PlanLimits {
    pub(crate) fn new(store: Entity<AppStore>) -> Self {
        Self {
            store,
            limits: Vec::new(),
            asked_at: None,
        }
    }

    /// Ask the agents again unless the last answers are under a minute old.
    pub(crate) fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        if self.asked_at.is_some_and(|at| at.elapsed() < FRESH_FOR) {
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let mut limits = Vec::new();
        for (agent, _) in LIMIT_AGENTS {
            if !limit_agent_available(self.store.read(cx), agent) {
                continue;
            }
            // Keep the last answer on screen while asking again.
            let last = self
                .limits
                .iter()
                .find(|(a, _)| *a == agent)
                .and_then(|(_, l)| l.clone());
            limits.push((agent, last));
            let ask = client.request(Request::PlanLimits {
                harness: HarnessId(agent.into()),
                refresh: false,
            });
            cx.spawn(async move |this, cx| {
                let result = ask.await;
                let _ = this.update(cx, |this, cx| {
                    let got = match result {
                        Ok(Response::AgentInfo { info }) => Ok(info),
                        Ok(_) => Err("The agent gave no report.".to_owned()),
                        Err(err) => Err(err.message),
                    };
                    if let Some(slot) = this.limits.iter_mut().find(|(a, _)| *a == agent) {
                        slot.1 = Some(got);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        if !limits.is_empty() {
            self.asked_at = Some(Instant::now());
        }
        self.limits = limits;
        cx.notify();
    }

    /// The most used limit across the agents, in percent.
    pub(crate) fn peak(&self) -> Option<f32> {
        self.limits
            .iter()
            .filter_map(|(_, l)| l.as_ref()?.as_ref().ok())
            .filter_map(crate::info_cards::peak_percent)
            .reduce(f32::max)
    }
}

fn note(text: impl Into<gpui_kit::SharedString>, p: &Palette) -> AnyElement {
    div()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(text.into())
        .into_any_element()
}

impl Render for PlanLimits {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let mut card = v_flex().gap_3();
        if self.limits.is_empty() {
            card = card.child(note(
                "Install Claude Code, Codex, or Antigravity to see plan limits.",
                &p,
            ));
        }
        for (agent, state) in &self.limits {
            let name = LIMIT_AGENTS
                .iter()
                .find(|(a, _)| a == agent)
                .map_or(*agent, |(_, n)| *n);
            let (plan, body) = match state {
                None => (
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
                Some(Err(why)) => (None, vec![note(why.clone(), &p)]),
                Some(Ok(info)) => {
                    let (plan, rows) = crate::info_cards::plan_limits(info, &p);
                    if rows.is_empty() {
                        (
                            plan,
                            vec![note("No plan limits (an API key pays per token).", &p)],
                        )
                    } else {
                        (plan, rows)
                    }
                }
            };
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(crate::brand::agent_logo(agent, px(14.), &p))
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name))
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
                    .children(body),
            );
        }
        // Wrapped so the margin keeps the card off the window's edge.
        div().child(
            v_flex()
                .m_3()
                .w(px(320.))
                .p_3()
                .gap_3()
                .font_family(cx.theme().font_family.clone())
                .text_color(p.foreground)
                .bg(p.elevated)
                .border_1()
                .border_color(p.border)
                .rounded(px(10.))
                .shadow_md()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Plan limits"),
                )
                .child(card)
                .when(!self.limits.is_empty(), |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child("Click for usage by day, agent and project"),
                    )
                }),
        )
    }
}
