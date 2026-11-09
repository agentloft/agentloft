//! The model picker: one popover for the agent, its model and how hard it
//! thinks. Search on top with the agents as tabs, the models on the left,
//! a reasoning-effort ladder for the highlighted model on the right.
//!
//! Keys: type to filter, ↑/↓ choose a model, ←/→ choose an effort (while
//! the search is empty), ⏎ applies both, esc closes.

use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{HarnessId, ModelInfo, ThreadId};

pub enum PickerEvent {
    Close,
}

pub struct ModelPicker {
    store: Entity<AppStore>,
    thread_id: ThreadId,
    search: Entity<InputState>,
    query: String,
    /// Index into the filtered list.
    highlighted: usize,
    /// Keyboard position in the effort ladder (0 = Auto); `None` follows
    /// the thread's setting.
    effort_cursor: Option<usize>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for ModelPicker {}

/// What the picker shows, read from the store each render.
struct Snapshot {
    harness: HarnessId,
    started: bool,
    current_model: Option<String>,
    current_effort: Option<String>,
    models: Vec<ModelInfo>,
    agents: Vec<(&'static str, &'static str)>,
}

impl ModelPicker {
    pub fn new(
        store: Entity<AppStore>,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search models…"));
        let subs = vec![
            cx.subscribe_in(&search, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().to_lowercase();
                    this.highlighted = 0;
                    this.effort_cursor = None;
                    cx.notify();
                }
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            thread_id,
            search,
            query: String::new(),
            highlighted: 0,
            effort_cursor: None,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    pub fn search_focus(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }

    /// Fresh state for an opening: empty search, the current model
    /// highlighted, its models fetched if they aren't yet.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.query.clear();
        self.effort_cursor = None;
        let snap = self.snapshot(cx);
        self.highlighted = snap
            .models
            .iter()
            .position(|m| is_current(m, snap.current_model.as_deref()))
            .unwrap_or(0);
        self.scroll.scroll_to_item(self.highlighted);
        let harness = snap.harness.clone();
        self.store.update(cx, |s, cx| s.ensure_models(harness, cx));
    }

    fn snapshot(&self, cx: &App) -> Snapshot {
        let store = self.store.read(cx);
        let thread = store
            .threads
            .get(&self.thread_id)
            .and_then(|t| t.read(cx).model.as_ref())
            .map(|m| (m.thread.clone(), !m.items.is_empty()));
        let (harness, started, current_model, current_effort) = match thread {
            Some((t, started)) => (t.harness, started, t.model, t.effort),
            None => (HarnessId::claude(), false, None, None),
        };
        let all = store.models.get(&harness).cloned().unwrap_or_default();
        let models = all
            .into_iter()
            .filter(|m| {
                self.query.is_empty()
                    || m.display_name.to_lowercase().contains(&self.query)
                    || m.id.to_lowercase().contains(&self.query)
                    || m.description
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(&self.query))
            })
            .collect();
        Snapshot {
            harness,
            started,
            current_model,
            current_effort,
            models,
            agents: store.available_harnesses(),
        }
    }

    fn move_model(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.snapshot(cx).models.len() as isize;
        if n == 0 {
            return;
        }
        self.highlighted = (self.highlighted as isize + delta).rem_euclid(n) as usize;
        self.effort_cursor = None;
        self.scroll.scroll_to_item(self.highlighted);
        cx.notify();
    }

    fn move_effort(&mut self, delta: isize, cx: &mut Context<Self>) {
        let snap = self.snapshot(cx);
        let Some(model) = snap.models.get(self.highlighted) else {
            return;
        };
        if model.efforts.is_empty() {
            return;
        }
        let levels = model.efforts.len() + 1; // + Auto
        let at = self
            .effort_cursor
            .unwrap_or_else(|| effort_index(model, &snap));
        self.effort_cursor = Some((at as isize + delta).clamp(0, levels as isize - 1) as usize);
        cx.notify();
    }

    /// ⏎: the highlighted model with the chosen effort.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let snap = self.snapshot(cx);
        let Some(model) = snap.models.get(self.highlighted).cloned() else {
            return;
        };
        let effort = match self.effort_cursor {
            Some(0) => Some(None),
            Some(i) => Some(model.efforts.get(i - 1).cloned()),
            None => None,
        };
        self.apply(&model, effort, &snap, cx);
        cx.emit(PickerEvent::Close);
    }

    /// Switch to `model`, and to `effort` when given (`Some(None)` = Auto).
    /// An effort the new model doesn't offer falls back to Auto.
    fn apply(
        &mut self,
        model: &ModelInfo,
        effort: Option<Option<String>>,
        snap: &Snapshot,
        cx: &mut Context<Self>,
    ) {
        let thread_id = self.thread_id.clone();
        let target = (!model.is_default).then(|| model.id.clone());
        let current = is_current(model, snap.current_model.as_deref());
        let effort = effort.unwrap_or_else(|| {
            snap.current_effort
                .clone()
                .filter(|e| model.efforts.contains(e))
        });
        let effort_changed = effort != snap.current_effort;
        self.store.update(cx, |s, cx| {
            if !current {
                s.set_model(thread_id.clone(), target, cx);
            }
            if effort_changed {
                s.set_effort(thread_id, effort, cx);
            }
        });
    }

    fn pick_model(&mut self, ix: usize, cx: &mut Context<Self>) {
        let snap = self.snapshot(cx);
        if let Some(model) = snap.models.get(ix).cloned() {
            self.highlighted = ix;
            self.effort_cursor = None;
            self.apply(&model, None, &snap, cx);
            // Stay open when there's an effort to choose next.
            if model.efforts.is_empty() {
                cx.emit(PickerEvent::Close);
            }
        }
    }

    fn pick_effort(&mut self, level: usize, cx: &mut Context<Self>) {
        let snap = self.snapshot(cx);
        if let Some(model) = snap.models.get(self.highlighted).cloned() {
            let effort = match level {
                0 => None,
                i => model.efforts.get(i - 1).cloned(),
            };
            self.apply(&model, Some(effort), &snap, cx);
            cx.emit(PickerEvent::Close);
        }
    }

    fn pick_agent(&mut self, harness: &'static str, snap: &Snapshot, cx: &mut Context<Self>) {
        if snap.started || snap.harness.as_str() == harness {
            return;
        }
        let (t, h) = (self.thread_id.clone(), HarnessId(harness.into()));
        self.store.update(cx, |s, cx| {
            s.ensure_models(h.clone(), cx);
            s.set_thread_harness(t, h, cx);
        });
        self.highlighted = 0;
        self.effort_cursor = None;
    }

    fn render_agents(&self, snap: &Snapshot, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut row = h_flex().gap(px(2.)).p(px(2.)).rounded(px(9.)).bg(p.muted);
        for &(id, name) in &snap.agents {
            let active = snap.harness.as_str() == id;
            let locked = snap.started && !active;
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("picker-agent-{id}")))
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(7.))
                    .text_xs()
                    .when(active, |d| {
                        d.bg(p.elevated)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .shadow_sm()
                    })
                    .when(!active && !locked, |d| {
                        d.cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .when(locked, |d| d.text_color(p.faint_foreground).opacity(0.6))
                    .child(crate::brand::agent_logo(id, px(13.), p))
                    .child(name)
                    .when(!locked, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            let snap = this.snapshot(cx);
                            this.pick_agent(id, &snap, cx);
                            cx.notify();
                        }))
                    }),
            );
        }
        row.into_any_element()
    }

    fn render_models(&self, snap: &Snapshot, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex()
            .id("picker-models")
            .flex_1()
            .min_w_0()
            .p_1()
            .gap_px()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        if snap.models.is_empty() {
            let text = if self.query.is_empty() {
                "Asking the agent for its models…"
            } else {
                "No models match."
            };
            return list
                .child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(p.faint_foreground)
                        .child(text),
                )
                .into_any_element();
        }
        // Pinned versions (`claude-opus-5`) after the family aliases.
        let mut pinned_header = false;
        for (ix, model) in snap.models.iter().enumerate() {
            let pinned = model.id.starts_with("claude-");
            let header = (pinned && !pinned_header).then(|| {
                pinned_header = true;
                h_flex()
                    .gap_1()
                    .items_center()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child(Icon::new(IconName::Clock).xsmall())
                    .child("Earlier versions")
            });
            let active = ix == self.highlighted;
            let current = is_current(model, snap.current_model.as_deref());
            let row = h_flex()
                .id(SharedString::from(format!("picker-model-{ix}")))
                .relative()
                .gap_2()
                .items_center()
                .px_2()
                .py(px(6.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(active, |d| d.bg(p.selected))
                .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                .child(
                    // Accent rail on the model in use.
                    div()
                        .w(px(3.))
                        .h(px(18.))
                        .rounded_full()
                        .when(current, |d| d.bg(p.accent)),
                )
                .child(crate::brand::agent_logo(snap.harness.as_str(), px(15.), p))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_sm()
                                .child(
                                    div()
                                        .font_weight(if current {
                                            FontWeight::SEMIBOLD
                                        } else {
                                            FontWeight::MEDIUM
                                        })
                                        .child(model.display_name.clone()),
                                )
                                .when(model.is_default, |d| d.child(tag("default", p))),
                        )
                        .when_some(model.description.clone(), |d, desc| {
                            d.child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(desc),
                            )
                        }),
                )
                .when(current, |d| {
                    d.child(Icon::new(IconName::Check).xsmall().text_color(p.accent))
                })
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if this.highlighted != ix {
                        this.highlighted = ix;
                        this.effort_cursor = None;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_model(ix, cx)));
            // Keep one child per model so `scroll_to_item` lines up.
            list = list.child(v_flex().children(header).child(row));
        }
        list.into_any_element()
    }

    fn render_efforts(&self, snap: &Snapshot, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let column = v_flex()
            .w(px(212.))
            .flex_none()
            .border_l_1()
            .border_color(p.border)
            .p_1();
        let Some(model) = snap.models.get(self.highlighted) else {
            return column.into_any_element();
        };
        let heading = h_flex()
            .justify_between()
            .items_center()
            .px_2()
            .pt_1p5()
            .pb_1()
            .text_xs()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.muted_foreground)
                    .child(Icon::new(IconName::Brain).xsmall())
                    .child("Reasoning effort"),
            )
            .child(
                div()
                    .truncate()
                    .text_color(p.faint_foreground)
                    .child(model.display_name.clone()),
            );
        if model.efforts.is_empty() {
            return column
                .child(heading)
                .child(
                    div()
                        .px_2()
                        .py_2()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child("This model doesn't take an effort setting."),
                )
                .into_any_element();
        }
        let selected = self
            .effort_cursor
            .unwrap_or_else(|| effort_index(model, snap));
        // What's applied now (only meaningful for the model in use).
        let applied =
            is_current(model, snap.current_model.as_deref()).then(|| effort_index(model, snap));
        let total = model.efforts.len();
        let default_level = model
            .default_effort
            .as_ref()
            .and_then(|d| model.efforts.iter().position(|e| e == d))
            .map(|i| i + 1);
        let mut ladder = v_flex()
            .id("picker-efforts")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_px();
        for level in 0..=total {
            let (label, hint) = if level == 0 {
                let default = model.default_effort.as_deref().map(effort_label);
                (
                    "Auto".to_owned(),
                    match default {
                        Some(d) => format!("Model default · {d}"),
                        None => "Model default".to_owned(),
                    },
                )
            } else {
                let e = &model.efforts[level - 1];
                let mut hint = effort_hint(e).to_owned();
                if model.default_effort.as_deref() == Some(e) {
                    hint = format!("{hint} · default");
                }
                (effort_label(e).to_owned(), hint)
            };
            let active = level == selected;
            ladder = ladder.child(
                h_flex()
                    .id(SharedString::from(format!("picker-effort-{level}")))
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py(px(5.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .when(active, |d| d.bg(p.selected))
                    .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                    .child(meter(level, total, default_level, p))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(label))
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(hint),
                            ),
                    )
                    .when(applied == Some(level), |d| {
                        d.child(Icon::new(IconName::Check).xsmall().text_color(p.accent))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_effort(level, cx))),
            );
        }
        column.child(heading).child(ladder).into_any_element()
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::input::{Enter, Escape, MoveDown, MoveLeft, MoveRight, MoveUp};
        let p = Palette::of(cx);
        let snap = self.snapshot(cx);
        if self.highlighted >= snap.models.len() {
            self.highlighted = snap.models.len().saturating_sub(1);
        }
        let footer_note = if snap.started {
            "Fork the thread (⋯) to use another agent"
        } else {
            "The agent is fixed once the thread starts"
        };
        v_flex()
            .w(px(580.))
            .rounded(px(14.))
            .overflow_hidden()
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                cx.stop_propagation();
                this.move_model(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                cx.stop_propagation();
                this.move_model(1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                if this.query.is_empty() {
                    cx.stop_propagation();
                    this.move_effort(-1, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveRight, _, cx| {
                if this.query.is_empty() {
                    cx.stop_propagation();
                    this.move_effort(1, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &Enter, _, cx| {
                cx.stop_propagation();
                this.confirm(cx);
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(PickerEvent::Close);
            }))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .prefix(Icon::new(IconName::Search).small())
                                .appearance(false)
                                .small(),
                        ),
                    )
                    .child(self.render_agents(&snap, &p, cx)),
            )
            .child(
                h_flex()
                    .h(px(330.))
                    .items_stretch()
                    .child(self.render_models(&snap, &p, cx))
                    .child(self.render_efforts(&snap, &p, cx)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(p.border)
                    .bg(p.muted)
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(keys("↑↓", "model", &p))
                            .child(keys("←→", "effort", &p))
                            .child(keys("⏎", "apply", &p)),
                    )
                    .child(div().truncate().child(footer_note)),
            )
    }
}

/// Whether `model` is what the thread runs (`None` = the default model).
fn is_current(model: &ModelInfo, current: Option<&str>) -> bool {
    match current {
        Some(c) => model.id == c || model.resolved_id.as_deref() == Some(c),
        None => model.is_default,
    }
}

/// The applied effort's ladder position (0 = Auto).
fn effort_index(model: &ModelInfo, snap: &Snapshot) -> usize {
    snap.current_effort
        .as_ref()
        .filter(|_| is_current(model, snap.current_model.as_deref()))
        .and_then(|e| model.efforts.iter().position(|x| x == e))
        .map_or(0, |i| i + 1)
}

pub fn effort_label(effort: &str) -> &str {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        "ultra" => "Ultra",
        other => other,
    }
}

fn effort_hint(effort: &str) -> &'static str {
    match effort {
        "none" | "minimal" => "Answers straight away",
        "low" => "Quick, light thinking",
        "medium" => "Balanced",
        "high" => "Thinks it through",
        "xhigh" => "For hard problems",
        "max" => "No limit on thinking",
        "ultra" => "Longest, deepest runs",
        _ => "",
    }
}

/// Rising bars, `level` of `total` lit. Auto (0) lights none but tints
/// the model's default level, when it has one.
pub fn meter(level: usize, total: usize, default: Option<usize>, p: &Palette) -> AnyElement {
    if level == 0 && default.is_none() {
        return div()
            .w(px(22.))
            .flex_none()
            .child(Icon::new(IconName::Sparkles).small().text_color(p.accent))
            .into_any_element();
    }
    let total = total.max(1);
    let mut bars = h_flex()
        .w(px(22.))
        .flex_none()
        .gap(px(2.))
        .items_end()
        .h(px(14.));
    for i in 1..=total {
        let h = 4. + 10. * (i as f32 / total as f32);
        let color = if level > 0 && i <= level {
            p.accent
        } else if level == 0 && default.is_some_and(|d| i <= d) {
            p.accent_soft
        } else {
            p.border
        };
        bars = bars.child(div().w(px(3.)).h(px(h)).rounded(px(1.)).bg(color));
    }
    bars.into_any_element()
}

fn tag(text: &'static str, p: &Palette) -> AnyElement {
    div()
        .px_1()
        .rounded(px(4.))
        .bg(p.accent_soft)
        .text_color(p.accent)
        .text_xs()
        .child(text)
        .into_any_element()
}

fn keys(k: &'static str, what: &'static str, p: &Palette) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(
            div()
                .px_1()
                .rounded(px(4.))
                .border_1()
                .border_color(p.border)
                .text_color(p.muted_foreground)
                .child(k),
        )
        .child(what)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, resolved: Option<&str>, default: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: id.into(),
            is_default: default,
            resolved_id: resolved.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn current_matches_alias_resolved_or_default() {
        let opus = model("opus", Some("claude-opus-5-5"), true);
        assert!(is_current(&opus, Some("opus")));
        assert!(is_current(&opus, Some("claude-opus-5-5")));
        assert!(is_current(&opus, None), "the default model");
        assert!(!is_current(&model("sonnet", None, false), None));
    }

    #[test]
    fn effort_labels() {
        assert_eq!(effort_label("xhigh"), "Extra high");
        assert_eq!(effort_label("turbo"), "turbo");
    }
}
