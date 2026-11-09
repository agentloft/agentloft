//! "Which agent, which model" for one-off jobs outside a thread (e.g.
//! designing a theme): agent tabs with logos over that agent's models.

use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::HarnessId;

pub enum AgentChoiceEvent {
    Close,
}

pub struct AgentChoice {
    store: Entity<AppStore>,
    pub harness: HarnessId,
    /// `None` = the agent's default model.
    pub model: Option<String>,
}

impl EventEmitter<AgentChoiceEvent> for AgentChoice {}

impl AgentChoice {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        let harness = HarnessId::claude();
        store.update(cx, |s, cx| s.ensure_models(harness.clone(), cx));
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            harness,
            model: None,
        }
    }

    /// The chosen model's name (or the agent's default one).
    pub fn model_name(&self, cx: &App) -> String {
        let store = self.store.read(cx);
        let models = store.models.get(&self.harness);
        let found = match &self.model {
            Some(m) => models.and_then(|ms| ms.iter().find(|x| &x.id == m)),
            None => models.and_then(|ms| ms.iter().find(|x| x.is_default)),
        };
        found
            .map(|m| m.display_name.clone())
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| "Default model".into())
    }

    fn pick_agent(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let h = HarnessId(id.into());
        if h != self.harness {
            self.store
                .update(cx, |s, cx| s.ensure_models(h.clone(), cx));
            self.harness = h;
            self.model = None;
            cx.notify();
        }
    }
}

impl Render for AgentChoice {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let (agents, models) = {
            let store = self.store.read(cx);
            (
                store.available_harnesses(),
                store.models.get(&self.harness).cloned().unwrap_or_default(),
            )
        };
        let mut tabs = h_flex().gap(px(2.)).p(px(2.)).rounded(px(9.)).bg(p.muted);
        for (id, name) in agents {
            let active = self.harness.as_str() == id;
            tabs = tabs.child(
                h_flex()
                    .id(SharedString::from(format!("choice-agent-{id}")))
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(7.))
                    .text_xs()
                    .cursor_pointer()
                    .when(active, |d| {
                        d.bg(p.elevated)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .shadow_sm()
                    })
                    .when(!active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .child(crate::brand::agent_logo(id, px(13.), &p))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_agent(id, cx))),
            );
        }
        let mut list = v_flex()
            .id("choice-models")
            .max_h(px(260.))
            .p_1()
            .gap_px()
            .overflow_y_scroll();
        let row = |id: String,
                   name: String,
                   detail: Option<String>,
                   chosen: bool,
                   value: Option<String>,
                   cx: &mut Context<Self>| {
            h_flex()
                .id(SharedString::from(format!("choice-model-{id}")))
                .gap_2()
                .items_center()
                .h(px(30.))
                .px_2()
                .rounded(px(7.))
                .text_sm()
                .cursor_pointer()
                .when(chosen, |d| d.bg(p.selected))
                .when(!chosen, |d| d.hover(|d| d.bg(p.hover)))
                .child(crate::brand::agent_logo(self.harness.as_str(), px(14.), &p))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::MEDIUM)
                        .child(name),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .children(detail),
                )
                .when(chosen, |d| {
                    d.child(Icon::new(IconName::Check).xsmall().text_color(p.accent))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.model = value.clone();
                    cx.emit(AgentChoiceEvent::Close);
                    cx.notify();
                }))
                .into_any_element()
        };
        let default_name = models
            .iter()
            .find(|m| m.is_default)
            .map(|m| m.display_name.clone());
        list = list.child(row(
            "default".into(),
            "Agent's default".into(),
            default_name,
            self.model.is_none(),
            None,
            cx,
        ));
        if models.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("Asking the agent for its models…"),
            );
        }
        for m in &models {
            let chosen = self.model.as_deref() == Some(m.id.as_str());
            list = list.child(row(
                m.id.clone(),
                m.display_name.clone(),
                m.description.clone(),
                chosen,
                Some(m.id.clone()),
                cx,
            ));
        }
        v_flex().w(px(420.)).gap_2().p_2().child(tabs).child(list)
    }
}

/// The chip that opens a chooser: logo, model name, chevron.
pub fn chip(choice: &AgentChoice, p: &Palette, cx: &App) -> AnyElement {
    h_flex()
        .gap_1p5()
        .items_center()
        .child(crate::brand::agent_logo(
            choice.harness.as_str(),
            px(13.),
            p,
        ))
        .child(div().text_color(p.foreground).child(choice.model_name(cx)))
        .child(
            Icon::new(IconName::ChevronDown)
                .xsmall()
                .text_color(p.faint_foreground),
        )
        .into_any_element()
}
