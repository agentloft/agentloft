//! Settings → Agents: every agent in a list (logo, version, on/off), and
//! for the selected one its defaults for new threads (model, reasoning,
//! permissions), how it runs (binary, arguments, environment) and which of
//! its models the pickers offer.

use super::SettingsView;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use loft_config::{HarnessConfig, SettingValue};
use loft_protocol::{HarnessId, ModelInfo, PermissionMode};

pub(super) const AGENTS: [(&str, &str); 4] = [
    (HarnessId::CLAUDE, "Claude Code"),
    (HarnessId::CODEX, "Codex"),
    (HarnessId::OPENCODE, "OpenCode"),
    (HarnessId::GEMINI, "Gemini CLI"),
];

/// Per-agent text fields, rebuilt when another agent is selected.
pub(super) struct AgentsPage {
    pub(super) selected: &'static str,
    built_for: Option<&'static str>,
    path: Option<Entity<InputState>>,
    args: Option<Entity<InputState>>,
    env: Vec<(Entity<InputState>, Entity<InputState>)>,
}

impl AgentsPage {
    pub(super) fn new() -> Self {
        Self {
            selected: HarnessId::CLAUDE,
            built_for: None,
            path: None,
            args: None,
            env: Vec::new(),
        }
    }
}

/// `codex-cli 0.167.0` → `0.167.0`; `2.1.319 (Claude Code)` → `2.1.319`.
fn short_version(v: &str) -> Option<String> {
    v.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_owned)
}

/// Label and hint, with the control underneath (the pane is narrow).
fn stacked(label: &str, hint: &str, control: AnyElement, p: &Palette) -> AnyElement {
    v_flex()
        .gap_2()
        .py_3()
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(hint.to_owned()),
                ),
        )
        .child(control)
        .into_any_element()
}

fn agent_config(config: &loft_config::Config, id: &str) -> HarnessConfig {
    match id {
        HarnessId::CODEX => config.harness.codex.clone(),
        HarnessId::OPENCODE => config.harness.opencode.clone(),
        HarnessId::GEMINI => config.harness.gemini.clone(),
        _ => config.harness.claude.clone(),
    }
}

/// Split launch arguments like a shell would (quotes group words).
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl SettingsView {
    fn agent_key(&self, field: &'static str) -> [&'static str; 3] {
        ["harness", self.agents.selected, field]
    }

    /// Text fields for the selected agent (built once per selection).
    fn ensure_agent_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.agents.selected;
        if self.agents.built_for == Some(id) {
            return;
        }
        let h = agent_config(&self.config, id);
        let text_field = |this: &mut Self,
                          value: String,
                          placeholder: &'static str,
                          window: &mut Window,
                          cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(placeholder);
                s.set_value(value, window, cx);
                let _ = this;
                s
            })
        };
        let path_value = if h.path == "auto" {
            String::new()
        } else {
            h.path.clone()
        };
        let path = text_field(self, path_value, "auto (login-shell PATH)", window, cx);
        self._subs
            .push(cx.subscribe_in(&path, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let text = state.read(cx).value().trim().to_owned();
                    let value = if text.is_empty() {
                        "auto".to_owned()
                    } else {
                        text
                    };
                    let key = this.agent_key("path");
                    if value != agent_config(&this.config, this.agents.selected).path {
                        this.write(&key, SettingValue::Str(value), true, cx);
                    }
                }
            }));
        let args = text_field(self, join_args(&h.extra_args), "none", window, cx);
        self._subs
            .push(cx.subscribe_in(&args, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let list = split_args(&state.read(cx).value());
                    if list != agent_config(&this.config, this.agents.selected).extra_args {
                        let key = this.agent_key("extra_args");
                        this.write(&key, SettingValue::List(list), true, cx);
                    }
                }
            }));
        let mut env = Vec::new();
        for (k, v) in &h.env {
            env.push(self.env_row(k.clone(), v.clone(), window, cx));
        }
        self.agents.path = Some(path);
        self.agents.args = Some(args);
        self.agents.env = env;
        self.agents.built_for = Some(id);
    }

    fn env_row(
        &mut self,
        key: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<InputState>, Entity<InputState>) {
        let k = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("NAME");
            s.set_value(key, window, cx);
            s
        });
        let v = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("value");
            s.set_value(value, window, cx);
            s
        });
        for input in [&k, &v] {
            self._subs
                .push(cx.subscribe_in(input, window, |this, _, event, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.save_env(cx);
                    }
                }));
        }
        (k, v)
    }

    fn save_env(&mut self, cx: &mut Context<Self>) {
        let pairs: Vec<(String, String)> = self
            .agents
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.read(cx).value().trim().to_owned(),
                    v.read(cx).value().to_string(),
                )
            })
            .filter(|(k, _)| !k.is_empty())
            .collect();
        let current: Vec<(String, String)> = agent_config(&self.config, self.agents.selected)
            .env
            .into_iter()
            .collect();
        let mut sorted = pairs.clone();
        sorted.sort();
        if sorted != current {
            let key = self.agent_key("env");
            let value = if pairs.is_empty() {
                SettingValue::Unset
            } else {
                SettingValue::Map(pairs)
            };
            self.write(&key, value, true, cx);
        }
    }

    pub(super) fn agents_page(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_agent_inputs(window, cx);
        let id = self.agents.selected;
        let store = self.store.read(cx);
        let statuses = store.harness_status.clone();
        let models: Vec<ModelInfo> = store
            .models
            .get(&HarnessId(id.into()))
            .cloned()
            .unwrap_or_default();

        // The list.
        let mut list = v_flex().w(px(176.)).flex_none().gap_1();
        for (aid, name) in AGENTS {
            let h = agent_config(&self.config, aid);
            let status = statuses.get(&HarnessId(aid.into()));
            let (line, ok) = match status {
                None => ("Checking…".to_owned(), true),
                Some(s) if s.installed => (
                    s.version
                        .as_deref()
                        .and_then(short_version)
                        .map(|v| format!("v{v}"))
                        .unwrap_or_else(|| "Installed".into()),
                    true,
                ),
                Some(_) => ("Not installed".to_owned(), false),
            };
            let active = aid == id;
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("agent-{aid}")))
                    .gap_2p5()
                    .items_center()
                    .px_3()
                    .py_2p5()
                    .rounded(px(10.))
                    .cursor_pointer()
                    .when(active, |d| d.bg(p.selected))
                    .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                    .child(crate::brand::agent_logo(aid, px(18.), p))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .when(!h.enabled, |d| d.text_color(p.muted_foreground))
                                    .child(name),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_xs()
                                    .text_color(p.muted_foreground)
                                    .child(div().size(px(6.)).rounded_full().bg(if !h.enabled {
                                        p.border
                                    } else if ok {
                                        p.success
                                    } else {
                                        p.warning
                                    }))
                                    .child(if h.enabled { line } else { "Off".into() }),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agents.selected = aid;
                        this.store
                            .update(cx, |s, cx| s.ensure_models(HarnessId(aid.into()), cx));
                        cx.notify();
                    })),
            );
        }

        // The selected agent.
        let name = AGENTS.iter().find(|(a, _)| *a == id).map_or("", |(_, n)| n);
        let h = agent_config(&self.config, id);
        let status = statuses.get(&HarnessId(id.into())).cloned();
        let enabled = h.enabled;
        let header = h_flex()
            .gap_3()
            .items_center()
            .pb_3()
            .border_b_1()
            .border_color(p.border)
            .child(crate::brand::agent_logo(id, px(28.), p))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(match &status {
                                Some(s) if s.installed => format!(
                                    "{} · {}",
                                    s.version
                                        .as_deref()
                                        .and_then(short_version)
                                        .map(|v| format!("v{v}"))
                                        .unwrap_or_else(|| "Installed".into()),
                                    s.path
                                        .as_deref()
                                        .map(crate::workspace::tilde_path_str)
                                        .unwrap_or_default()
                                ),
                                Some(s) => {
                                    s.error.clone().unwrap_or_else(|| "Not installed".into())
                                }
                                None => "Checking…".into(),
                            }),
                    ),
            )
            .child(
                div()
                    .id("agent-recheck")
                    .flex_none()
                    .p(px(5.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(IconName::RefreshCw).small())
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Check again").build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.probe_harnesses(cx));
                    })),
            )
            .child(
                Switch::new("agent-enabled")
                    .checked(enabled)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        let key = this.agent_key("enabled");
                        let v = if *on {
                            SettingValue::Unset
                        } else {
                            SettingValue::Bool(false)
                        };
                        this.write(&key, v, false, cx);
                    })),
            );

        let default_model_id = (h.default_model != "auto").then(|| h.default_model.clone());
        let effective_model = match &default_model_id {
            Some(m) => models
                .iter()
                .find(|x| &x.id == m || x.resolved_id.as_deref() == Some(m.as_str())),
            None => models.iter().find(|x| x.is_default),
        };
        let model_menu = self.default_model_menu(&models, default_model_id.as_deref(), p, cx);
        let efforts = effective_model
            .map(|m| m.efforts.clone())
            .unwrap_or_default();
        let effort_ctl = if efforts.is_empty() {
            div()
                .text_xs()
                .text_color(p.muted_foreground)
                .child("This model has no effort setting.")
                .into_any_element()
        } else {
            let mut options: Vec<(String, String)> = vec![("auto".into(), "Auto".into())];
            options.extend(
                efforts
                    .iter()
                    .map(|e| (e.clone(), crate::model_picker::effort_label(e).to_owned())),
            );
            let mut seg = h_flex().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(p.muted);
            for (value, label) in options {
                let active = h.default_effort == value;
                seg = seg.child(
                    div()
                        .id(SharedString::from(format!("default-effort-{value}")))
                        .px_2()
                        .py_1()
                        .rounded(px(6.))
                        .text_xs()
                        .cursor_pointer()
                        .when(active, |d| {
                            d.bg(p.background)
                                .text_color(p.foreground)
                                .font_weight(FontWeight::MEDIUM)
                                .shadow_sm()
                        })
                        .when(!active, |d| {
                            d.text_color(p.muted_foreground)
                                .hover(|d| d.text_color(p.foreground))
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let key = this.agent_key("default_effort");
                            let v = if value == "auto" {
                                SettingValue::Unset
                            } else {
                                SettingValue::Str(value.clone())
                            };
                            this.write(&key, v, true, cx);
                        })),
                );
            }
            seg.into_any_element()
        };

        // Permissions: the same icon strip as the composer picker.
        let current_mode: Option<PermissionMode> =
            serde_json::from_value(serde_json::json!(h.default_permission_mode)).ok();
        let mut strip = h_flex().gap(px(2.)).p(px(2.)).rounded(px(8.)).bg(p.muted);
        for mode in crate::model_picker::MODES {
            let active = mode == current_mode;
            strip = strip.child(
                div()
                    .id(SharedString::from(format!(
                        "default-mode-{}",
                        crate::model_picker::mode_short(mode)
                    )))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(30.))
                    .h(px(24.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(active, |d| {
                        d.bg(p.background).text_color(p.accent).shadow_sm()
                    })
                    .when(!active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .child(Icon::new(crate::model_picker::mode_icon(mode)).small())
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(crate::workspace::mode_label(
                            mode,
                        ))
                        .build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let key = this.agent_key("default_permission_mode");
                        let v = match mode {
                            None => SettingValue::Unset,
                            Some(m) => SettingValue::Str(
                                serde_json::to_value(m)
                                    .ok()
                                    .and_then(|v| v.as_str().map(str::to_owned))
                                    .unwrap_or_default(),
                            ),
                        };
                        this.write(&key, v, true, cx);
                    })),
            );
        }
        let permissions = h_flex().gap_2().items_center().child(strip).child(
            div()
                .text_xs()
                .text_color(p.muted_foreground)
                .child(crate::workspace::mode_label(current_mode)),
        );

        // Environment rows.
        let mut env = v_flex().gap_1p5();
        for (ix, (k, v)) in self.agents.env.iter().enumerate() {
            env = env.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(150.)).child(Input::new(k).small()))
                    .child(div().text_color(p.muted_foreground).child("="))
                    .child(div().flex_1().child(Input::new(v).small()))
                    .child(
                        div()
                            .id(("env-remove", ix))
                            .p(px(4.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if ix < this.agents.env.len() {
                                    this.agents.env.remove(ix);
                                    this.save_env(cx);
                                }
                            })),
                    ),
            );
        }
        env = env.child(
            h_flex()
                .id("env-add")
                .gap_1()
                .items_center()
                .text_sm()
                .text_color(p.muted_foreground)
                .cursor_pointer()
                .hover(|d| d.text_color(p.foreground))
                .child(Icon::new(IconName::Plus).xsmall())
                .child("Add variable")
                .on_click(cx.listener(|this, _, window, cx| {
                    let row = this.env_row(String::new(), String::new(), window, cx);
                    row.0.update(cx, |s, cx| s.focus(window, cx));
                    this.agents.env.push(row);
                    cx.notify();
                })),
        );

        // Models shown in the pickers.
        let hidden = h.hidden_models.clone();
        let mut model_rows = v_flex().gap_px();
        if models.is_empty() {
            model_rows = model_rows.child(
                div()
                    .py_2()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child("Asking the agent for its models…"),
            );
        }
        for m in &models {
            let shown = !hidden.contains(&m.id);
            let mid = m.id.clone();
            model_rows = model_rows.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .h(px(34.))
                    .px_2()
                    .rounded(px(7.))
                    .hover(|d| d.bg(p.hover))
                    .child(crate::brand::agent_logo(id, px(14.), p))
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .when(!shown, |d| d.text_color(p.muted_foreground))
                            .child(m.display_name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(p.mono.clone())
                            .text_color(p.faint_foreground)
                            .child(m.id.clone()),
                    )
                    .child(div().flex_1())
                    .when(!m.efforts.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child(format!("{} efforts", m.efforts.len())),
                        )
                    })
                    .child(
                        Switch::new(SharedString::from(format!("show-{}", m.id)))
                            .small()
                            .checked(shown)
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                let mut hidden =
                                    agent_config(&this.config, this.agents.selected).hidden_models;
                                hidden.retain(|x| x != &mid);
                                if !on {
                                    hidden.push(mid.clone());
                                }
                                let key = this.agent_key("hidden_models");
                                let v = if hidden.is_empty() {
                                    SettingValue::Unset
                                } else {
                                    SettingValue::List(hidden)
                                };
                                this.write(&key, v, false, cx);
                            })),
                    ),
            );
        }

        let section = |title: &'static str, hint: &'static str| {
            v_flex()
                .pt_5()
                .pb_1()
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(div().text_xs().text_color(p.muted_foreground).child(hint))
        };
        let detail = v_flex()
            .flex_1()
            .min_w_0()
            .p_5()
            .rounded(px(14.))
            .border_1()
            .border_color(p.border)
            .bg(p.muted.opacity(0.4))
            .child(header)
            .when(!enabled, |d| {
                d.child(
                    div()
                        .mt_3()
                        .px_3()
                        .py_2()
                        .rounded(px(8.))
                        .bg(p.muted)
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Off: hidden from pickers. Threads that use it keep working."),
                )
            })
            .child(section(
                "Defaults for new threads",
                "Change any of these per thread from the composer.",
            ))
            .child(stacked(
                "Model",
                "What new threads start with.",
                h_flex().child(model_menu).into_any_element(),
                p,
            ))
            .child(stacked(
                "Reasoning effort",
                "How long it thinks; Auto leaves it to the model.",
                effort_ctl,
                p,
            ))
            .child(stacked(
                "Permissions",
                "How much it may do without asking.",
                permissions.into_any_element(),
                p,
            ))
            .child(section("Runtime", "How Agentloft starts this agent."))
            .child(stacked(
                "Binary",
                "Leave empty to find it on your login shell's PATH.",
                self.agents
                    .path
                    .as_ref()
                    .map(|i| {
                        div()
                            .w_full()
                            .child(Input::new(i).small())
                            .into_any_element()
                    })
                    .unwrap_or_else(|| div().into_any_element()),
                p,
            ))
            .child(stacked(
                "Launch arguments",
                "Extra command-line arguments, added to every session.",
                self.agents
                    .args
                    .as_ref()
                    .map(|i| {
                        div()
                            .w_full()
                            .child(Input::new(i).small())
                            .into_any_element()
                    })
                    .unwrap_or_else(|| div().into_any_element()),
                p,
            ))
            .child(
                v_flex()
                    .gap_2()
                    .py_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Environment"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child("API keys, base URLs and other variables for this agent only."),
                    )
                    .child(env),
            )
            .child(section(
                "Models",
                "Switch off the ones you don't want in the pickers.",
            ))
            .child(model_rows);

        h_flex()
            .items_start()
            .gap_4()
            .pt_2()
            .child(list)
            .child(detail)
            .into_any_element()
    }

    fn default_model_menu(
        &self,
        models: &[ModelInfo],
        current: Option<&str>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_kit::component::button::Button;
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let label = match current {
            Some(m) => models
                .iter()
                .find(|x| x.id == m || x.resolved_id.as_deref() == Some(m))
                .map(|x| x.display_name.clone())
                .unwrap_or_else(|| m.to_owned()),
            None => match models.iter().find(|x| x.is_default) {
                Some(d) => format!("Agent's default ({})", d.display_name),
                None => "Agent's default".to_owned(),
            },
        };
        let options: Vec<(String, String)> = models
            .iter()
            .map(|m| (m.id.clone(), m.display_name.clone()))
            .collect();
        let current = current.map(str::to_owned);
        let view = cx.entity();
        let id = self.agents.selected;
        let _ = p;
        Button::new("default-model")
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let v = view.clone();
                menu = menu
                    .item(
                        PopupMenuItem::new("Agent's default")
                            .checked(current.is_none())
                            .on_click(move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.write(
                                        &["harness", id, "default_model"],
                                        SettingValue::Unset,
                                        true,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .separator();
                for (mid, name) in &options {
                    let (v, mid) = (view.clone(), mid.clone());
                    let checked = current.as_deref() == Some(mid.as_str());
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(checked).on_click(
                        move |_, _, cx| {
                            let mid = mid.clone();
                            v.update(cx, |this, cx| {
                                this.write(
                                    &["harness", id, "default_model"],
                                    SettingValue::Str(mid),
                                    true,
                                    cx,
                                );
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_arguments_round_trip() {
        let args = split_args(r#"--foo "two words" 'x y' bare"#);
        assert_eq!(args, ["--foo", "two words", "x y", "bare"]);
        assert_eq!(join_args(&args), r#"--foo "two words" "x y" bare"#);
        assert!(split_args("   ").is_empty());
        assert_eq!(
            short_version("codex-cli 0.167.0").as_deref(),
            Some("0.167.0")
        );
        assert_eq!(
            short_version("2.1.319 (Claude Code)").as_deref(),
            Some("2.1.319")
        );
        assert_eq!(short_version("opencode v2.2.5").as_deref(), Some("2.2.5"));
    }
}
