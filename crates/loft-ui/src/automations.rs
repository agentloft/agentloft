//! The Automations page (sidebar ▸ Automations): prompts the daemon runs on
//! a schedule, each run a new thread. A list with each automation's
//! schedule, next run, recent runs and switches, and an editor for a new
//! or changed one. Agents manage the same automations over MCP.

use crate::store::AppStore;
use crate::theme::Palette;
use chrono::{Datelike as _, Local, TimeZone as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{
    Automation, AutomationId, AutomationSpec, HarnessId, PermissionMode, ProjectId, Request,
    Schedule, Thread, ThreadStatus,
};
use std::collections::HashSet;

/// How many of an automation's runs its card lists.
const RECENT_RUNS: usize = 3;

pub struct AutomationsPage {
    store: Entity<AppStore>,
    /// A new automation or one being changed.
    editor: Option<Editor>,
    /// Delete is asking "sure?" for this one.
    confirm_delete: Option<AutomationId>,
    /// "Run now" asked, not answered yet.
    starting: HashSet<AutomationId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Interval,
    Daily,
    Cron,
    /// A one-off (made by an agent): kept as it is.
    Once(i64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Minutes,
    Hours,
    Days,
}

impl Unit {
    fn minutes(self) -> u32 {
        match self {
            Unit::Minutes => 1,
            Unit::Hours => 60,
            Unit::Days => 1440,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Unit::Minutes => "minutes",
            Unit::Hours => "hours",
            Unit::Days => "days",
        }
    }
}

struct Editor {
    id: Option<AutomationId>,
    name: Entity<InputState>,
    prompt: Entity<TextareaState>,
    /// Which project: a list you can type in to search.
    project: Entity<SelectState<SearchableVec<ProjectItem>>>,
    kind: Kind,
    every: Entity<InputState>,
    unit: Unit,
    time: Entity<InputState>,
    /// Sunday first.
    days: [bool; 7],
    cron: Entity<InputState>,
    harness: Option<HarnessId>,
    model: Option<String>,
    effort: Option<String>,
    permission: Option<PermissionMode>,
    worktree: bool,
    enabled: bool,
    error: Option<String>,
    saving: bool,
}

impl AutomationsPage {
    pub fn new(store: Entity<AppStore>) -> Self {
        Self {
            store,
            editor: None,
            confirm_delete: None,
            starting: HashSet::new(),
        }
    }

    /// Open the editor on `automation`, or on a new one.
    fn edit(
        &mut self,
        automation: Option<&Automation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let default_project = store
            .selected
            .as_ref()
            .and_then(|id| store.workspace.thread(id))
            .map(|t| t.project_id.clone())
            .or_else(|| {
                store
                    .workspace
                    .projects
                    .iter()
                    .find(|p| !store.is_chats(p))
                    .map(|p| p.id.clone())
            });
        let default_harness = store.default_harness.clone();
        // Projects by name, a chat last.
        let mut items: Vec<ProjectItem> = store
            .workspace
            .projects
            .iter()
            .map(|pr| ProjectItem {
                id: pr.id.clone(),
                name: if store.is_chats(pr) {
                    "No project (a chat)".into()
                } else {
                    pr.name.clone().into()
                },
                path: crate::workspace::tilde_path_str(&pr.root_path).to_string(),
                chat: store.is_chats(pr),
            })
            .collect();
        items.sort_by_key(|i| (i.chat, i.name.to_lowercase()));
        let input = |value: String,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(placeholder);
                s.set_value(value, window, cx);
                s
            })
        };
        let spec = automation.map(|a| a.spec.clone());
        let (kind, every, unit, time, days, cron) = match spec.as_ref().map(|s| &s.schedule) {
            Some(Schedule::Interval { minutes }) => {
                let unit = if minutes % 1440 == 0 {
                    Unit::Days
                } else if minutes % 60 == 0 {
                    Unit::Hours
                } else {
                    Unit::Minutes
                };
                (
                    Kind::Interval,
                    (minutes / unit.minutes()).to_string(),
                    unit,
                    "09:00".to_owned(),
                    [true; 7],
                    String::new(),
                )
            }
            Some(Schedule::Daily { time, days }) => {
                let mut on = [days.is_empty(); 7];
                for d in days {
                    on[(*d % 7) as usize] = true;
                }
                (
                    Kind::Daily,
                    "1".into(),
                    Unit::Hours,
                    time.clone(),
                    on,
                    String::new(),
                )
            }
            Some(Schedule::Cron { expr }) => (
                Kind::Cron,
                "1".into(),
                Unit::Hours,
                "09:00".into(),
                [true; 7],
                expr.clone(),
            ),
            Some(Schedule::Once { at_ms }) => (
                Kind::Once(*at_ms),
                "1".into(),
                Unit::Hours,
                "09:00".into(),
                [true; 7],
                String::new(),
            ),
            // New: weekdays at nine.
            None => (
                Kind::Daily,
                "1".into(),
                Unit::Hours,
                "09:00".into(),
                [false, true, true, true, true, true, false],
                String::new(),
            ),
        };
        let want = spec
            .as_ref()
            .map(|s| s.project_id.clone())
            .or(default_project);
        let at = want
            .as_ref()
            .and_then(|w| items.iter().position(|i| &i.id == w))
            .map(|row| gpui_kit::component::IndexPath::default().row(row));
        let project = cx
            .new(|cx| SelectState::new(SearchableVec::new(items), at, window, cx).searchable(true));
        let prompt_text = spec.as_ref().map(|s| s.prompt.clone()).unwrap_or_default();
        let prompt = cx.new(|cx| {
            let mut s = TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .placeholder("What should the agent do on each run? It starts fresh every time, so say everything it needs.");
            s.set_value(prompt_text, window, cx);
            s
        });
        let editor = Editor {
            id: automation.map(|a| a.id.clone()),
            name: input(
                spec.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
                "e.g. Morning dependency check",
                window,
                cx,
            ),
            prompt,
            project,
            kind,
            every: input(every, "1", window, cx),
            unit,
            time: input(time, "09:00", window, cx),
            days,
            cron: input(cron, "0 9 * * 1-5", window, cx),
            harness: spec
                .as_ref()
                .map(|s| s.harness.clone())
                .unwrap_or(Some(default_harness)),
            model: spec.as_ref().and_then(|s| s.model.clone()),
            effort: spec.as_ref().and_then(|s| s.effort.clone()),
            permission: spec.as_ref().and_then(|s| s.permission_mode),
            worktree: spec.as_ref().is_some_and(|s| s.worktree),
            enabled: spec.as_ref().is_none_or(|s| s.enabled),
            error: None,
            saving: false,
        };
        if let Some(h) = editor.harness.clone() {
            self.store.update(cx, |s, cx| s.ensure_models(h, cx));
        }
        editor.name.update(cx, |s, cx| s.focus(window, cx));
        self.editor = Some(editor);
        self.confirm_delete = None;
        cx.notify();
    }

    /// The editor's spec, or what is wrong with it.
    fn spec(&self, cx: &Context<Self>) -> Result<(Option<AutomationId>, AutomationSpec), String> {
        let e = self.editor.as_ref().ok_or("nothing to save")?;
        let text = |i: &Entity<InputState>| i.read(cx).value().trim().to_owned();
        let schedule = match e.kind {
            Kind::Interval => {
                let n: u32 = text(&e.every)
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("How often: a whole number, 1 or more")?;
                Schedule::Interval {
                    minutes: n.saturating_mul(e.unit.minutes()),
                }
            }
            Kind::Daily => {
                if !e.days.iter().any(|d| *d) {
                    return Err("Pick at least one day".into());
                }
                Schedule::Daily {
                    time: text(&e.time),
                    days: if e.days.iter().all(|d| *d) {
                        Vec::new()
                    } else {
                        (0..7u8).filter(|d| e.days[*d as usize]).collect()
                    },
                }
            }
            Kind::Cron => Schedule::Cron {
                expr: text(&e.cron),
            },
            Kind::Once(at_ms) => Schedule::Once { at_ms },
        };
        Ok((
            e.id.clone(),
            AutomationSpec {
                name: text(&e.name),
                prompt: e.prompt.read(cx).value().trim().to_owned(),
                project_id: e
                    .project
                    .read(cx)
                    .selected_value()
                    .cloned()
                    .ok_or("Pick a project")?,
                schedule,
                enabled: e.enabled,
                harness: e.harness.clone(),
                model: e.model.clone(),
                effort: e.effort.clone(),
                permission_mode: e.permission,
                worktree: e.worktree,
            },
        ))
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (id, spec) = match self.spec(cx) {
            Ok(s) => s,
            Err(err) => {
                if let Some(e) = &mut self.editor {
                    e.error = Some(err);
                }
                cx.notify();
                return;
            }
        };
        if let Some(e) = &mut self.editor {
            e.saving = true;
            e.error = None;
        }
        cx.notify();
        crate::store::request_in(
            &self.store,
            Request::SaveAutomation {
                id,
                spec,
                from: None,
            },
            cx,
            |this, result, cx| {
                match result {
                    Ok(_) => this.editor = None,
                    Err(err) => {
                        if let Some(e) = &mut this.editor {
                            e.saving = false;
                            e.error = Some(capitalized(&err.message));
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    fn run_now(&mut self, id: AutomationId, cx: &mut Context<Self>) {
        if !self.starting.insert(id.clone()) {
            return;
        }
        cx.notify();
        let key = id.clone();
        crate::store::request_in(
            &self.store,
            Request::RunAutomation { id },
            cx,
            move |this, result, cx| {
                this.starting.remove(&key);
                if let Err(err) = result {
                    this.store.update(cx, |s, cx| {
                        s.fail(format!("Couldn't run it: {}", err.message), cx)
                    });
                }
                cx.notify();
            },
        );
    }

    /// Dev hook (`AGENTLOFT_DEV_TAB=automations:new|edit`).
    pub(crate) fn dev_open(&mut self, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        match what {
            "new" => self.edit(None, window, cx),
            "edit" => {
                let first = self.store.read(cx).workspace.automations.first().cloned();
                self.edit(first.as_ref(), window, cx);
            }
            _ => {}
        }
    }

    fn editor_mut(&mut self, f: impl FnOnce(&mut Editor), cx: &mut Context<Self>) {
        if let Some(e) = &mut self.editor {
            f(e);
            e.error = None;
        }
        cx.notify();
    }
}

impl Render for AutomationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let (automations, threads) = {
            let store = self.store.read(cx);
            (
                store.workspace.automations.clone(),
                store
                    .workspace
                    .threads
                    .iter()
                    .filter(|t| t.automation_id.is_some())
                    .map(Thread::without_commands)
                    .collect::<Vec<_>>(),
            )
        };
        // An automation being changed that has since gone: close it.
        if let Some(id) = self.editor.as_ref().and_then(|e| e.id.clone())
            && !automations.iter().any(|a| a.id == id)
        {
            self.editor = None;
        }
        let editing = self.editor.as_ref().map(|e| e.id.clone());
        let mut column = v_flex()
            .w_full()
            .max_w(px(760.))
            .mx_auto()
            .px_6()
            .pt_8()
            .pb_16()
            .gap_4()
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Automations"),
                            )
                            .child(div().text_sm().text_color(p.muted_foreground).child(
                                "Prompts that run on a schedule. Each run starts a new thread, which shows in the sidebar like any other. Agents can set these up for you too.",
                            )),
                    )
                    .when(editing.is_none(), |d| {
                        d.child(
                            div().flex_none().child(
                            Button::new("automation-new")
                                .small()
                                .cursor_pointer()
                                .primary()
                                .icon(IconName::Plus)
                                .label("New automation")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.edit(None, window, cx)
                                })),
                            ),
                        )
                    }),
            );
        if editing == Some(None) {
            column = column.child(self.render_editor(&p, cx));
        }
        if automations.is_empty() && editing.is_none() {
            column = column.child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .py_12()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(p.border)
                    .child(
                        Icon::new(IconName::Clock)
                            .size(px(28.))
                            .text_color(p.faint_foreground),
                    )
                    .child(div().text_sm().child("No automations yet"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child("A daily dependency check, a nightly test run, a weekly summary of what changed…"),
                    ),
            );
        }
        for a in &automations {
            if editing.as_ref() == Some(&Some(a.id.clone())) {
                column = column.child(self.render_editor(&p, cx));
                continue;
            }
            let mut runs: Vec<&Thread> = threads
                .iter()
                .filter(|t| t.automation_id.as_ref() == Some(&a.id))
                .collect();
            runs.sort_by_key(|t| std::cmp::Reverse(t.created_at_ms));
            let total = runs.len();
            runs.truncate(RECENT_RUNS);
            column = column.child(self.render_card(a, &runs, total, &p, cx));
        }
        div()
            .id("automations-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

impl AutomationsPage {
    fn render_card(
        &self,
        a: &Automation,
        runs: &[&Thread],
        total: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let project = store
            .workspace
            .projects
            .iter()
            .find(|pr| pr.id == a.spec.project_id)
            .map(|pr| {
                if store.is_chats(pr) {
                    "No project".to_owned()
                } else {
                    pr.name.clone()
                }
            })
            .unwrap_or_else(|| "Missing project".into());
        let agent = a
            .spec
            .harness
            .clone()
            .unwrap_or_else(|| store.default_harness.clone());
        let agent_name = agent_name(&agent);
        let on = a.spec.enabled;
        let id = a.id.clone();
        let starting = self.starting.contains(&a.id);
        let confirming = self.confirm_delete.as_ref() == Some(&a.id);

        let when = match (on, a.next_run_at_ms) {
            (true, Some(at)) => format!("Next run {}", when_text(at)),
            (true, None) => "No run ahead".into(),
            (false, _) => "Paused".into(),
        };
        let mut meta = h_flex()
            .gap_1p5()
            .items_center()
            .flex_wrap()
            .text_xs()
            .text_color(p.muted_foreground)
            .child(describe(&a.spec.schedule))
            .child("·")
            .child(project)
            .child("·")
            .child(crate::brand::agent_logo(agent.as_str(), px(12.), p))
            .child(agent_name);
        if a.spec.worktree {
            meta = meta
                .child("·")
                .child(Icon::new(IconName::GitBranch).xsmall())
                .child("Worktree");
        }

        let (toggle_id, run_id, edit_id, del_id, yes_id, no_id) = (
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
        );
        let card_key = a.id.to_string();
        let edit_from = a.clone();
        let actions = if confirming {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .mr_1()
                        .child("Delete it? Its threads stay."),
                )
                .child(
                    Button::new(SharedString::from(format!("automation-del-no-{card_key}")))
                        .xsmall()
                        .cursor_pointer()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let _ = &no_id;
                            this.confirm_delete = None;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("automation-del-yes-{card_key}")))
                        .xsmall()
                        .cursor_pointer()
                        .danger()
                        .label("Delete")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = None;
                            let id = yes_id.clone();
                            this.store.update(cx, |s, cx| {
                                s.request(Request::DeleteAutomation { id }, cx, |_, _, _| {})
                            });
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new(SharedString::from(format!("automation-run-{card_key}")))
                        .xsmall()
                        .cursor_pointer()
                        .outline()
                        .loading(starting)
                        .label(if starting { "Starting…" } else { "Run now" })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.run_now(run_id.clone(), cx)),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("automation-edit-{card_key}")))
                        .xsmall()
                        .cursor_pointer()
                        .ghost()
                        .icon(IconName::SquarePen)
                        .tooltip("Edit")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let _ = &edit_id;
                            this.edit(Some(&edit_from), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("automation-delete-{card_key}")))
                        .xsmall()
                        .cursor_pointer()
                        .ghost()
                        .icon(IconName::Trash)
                        .tooltip("Delete")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = Some(del_id.clone());
                            cx.notify();
                        })),
                )
                .child(
                    div().pl_1().child(
                        Switch::new(SharedString::from(format!("automation-on-{card_key}")))
                            .cursor_pointer()
                            .checked(on)
                            .tooltip(if on { "Pause" } else { "Resume" })
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let (id, enabled) = (toggle_id.clone(), *checked);
                                this.store.update(cx, |s, cx| {
                                    s.request(
                                        Request::SetAutomationEnabled { id, enabled },
                                        cx,
                                        |_, _, _| {},
                                    )
                                });
                            })),
                    ),
                )
                .into_any_element()
        };

        let mut card = v_flex()
            .rounded(px(12.))
            .border_1()
            .border_color(p.border)
            .bg(p.elevated)
            .overflow_hidden()
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .px_4()
                    .py_3()
                    .child(
                        div()
                            .mt(px(2.))
                            .size(px(30.))
                            .flex_none()
                            .rounded(px(8.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(if on { p.accent.opacity(0.14) } else { p.hover })
                            .text_color(if on { p.accent } else { p.muted_foreground })
                            .child(Icon::new(IconName::Clock).small()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .when(!on, |d| d.text_color(p.muted_foreground))
                                    .child(a.spec.name.clone()),
                            )
                            .child(meta)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if on { p.foreground } else { p.faint_foreground })
                                    .child(when),
                            )
                            .when_some(a.last_error.clone(), |d, err| {
                                d.child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .text_xs()
                                        .text_color(p.danger)
                                        .child(Icon::new(IconName::CircleAlert).xsmall())
                                        .child(div().min_w_0().truncate().child(err)),
                                )
                            }),
                    )
                    .child(actions),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .pl(px(58.))
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .line_clamp(2)
                    .child(a.spec.prompt.clone()),
            );
        if !runs.is_empty() {
            let mut list = v_flex()
                .border_t_1()
                .border_color(p.border.opacity(0.6))
                .py_1();
            for t in runs {
                list = list.child(self.render_run(t, p, cx));
            }
            if total > runs.len() {
                list = list.child(
                    div()
                        .pl(px(58.))
                        .py_1()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(format!(
                            "{} earlier runs in the sidebar",
                            total - runs.len()
                        )),
                );
            }
            card = card.child(list);
        }
        card.into_any_element()
    }

    fn render_run(&self, t: &Thread, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (dot, status) = match t.status {
            ThreadStatus::Running => (p.accent, "Running"),
            ThreadStatus::WaitingApproval => (p.warning, "Waiting for you"),
            ThreadStatus::Error => (p.danger, "Failed"),
            ThreadStatus::Archived => (p.faint_foreground, "Archived"),
            ThreadStatus::Idle if t.is_waiting() => (p.accent, "Running"),
            ThreadStatus::Idle => (p.success, "Done"),
        };
        let id = t.id.clone();
        let archived = t.status == ThreadStatus::Archived;
        h_flex()
            .id(SharedString::from(format!("automation-run-row-{}", t.id)))
            .gap_2()
            .items_center()
            .pl(px(58.))
            .pr_4()
            .py_1()
            .text_xs()
            .when(!archived, |d| {
                d.cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |s, cx| s.select_thread(id, cx));
                    }))
            })
            .child(div().size(px(6.)).rounded_full().bg(dot))
            .child(div().text_color(p.foreground).child(status))
            .child(
                div()
                    .text_color(p.muted_foreground)
                    .child(started_text(t.created_at_ms)),
            )
            .child(div().flex_1())
            .when(!archived, |d| {
                d.child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(p.faint_foreground),
                )
            })
            .into_any_element()
    }

    fn render_editor(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(e) = &self.editor else {
            return div().into_any_element();
        };
        let view = cx.entity();
        let (agents, models) = {
            let store = self.store.read(cx);
            let models: Vec<(String, String)> = e
                .harness
                .as_ref()
                .and_then(|h| store.models.get(h))
                .map(|m| {
                    m.iter()
                        .map(|m| (m.id.clone(), m.display_name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (store.available_harnesses(), models)
        };
        let label = |text: &'static str| {
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.muted_foreground)
                .child(text)
        };
        let field = |name: &'static str, body: AnyElement| {
            v_flex().gap_1p5().child(label(name)).child(body)
        };

        // Project: type to search by name or folder.
        let project_btn = Select::new(&e.project)
            .small()
            .cursor_pointer()
            .w(px(220.))
            .icon(IconName::Folder)
            .placeholder("Pick a project")
            .search_placeholder("Search projects")
            .menu_width(px(320.))
            .menu_max_h(px(320.));

        // Schedule.
        let kinds = [
            (Kind::Daily, "At a time"),
            (Kind::Interval, "Every…"),
            (Kind::Cron, "Cron"),
        ];
        let mut segmented = h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(p.hover)
            .w_auto();
        let once = matches!(e.kind, Kind::Once(_));
        for (kind, name) in kinds.into_iter().chain(once.then_some((e.kind, "Once"))) {
            let selected = e.kind == kind;
            segmented = segmented.child(
                div()
                    .id(SharedString::from(format!("automation-kind-{name}")))
                    .px_3()
                    .py_1()
                    .rounded(px(6.))
                    .text_xs()
                    .cursor_pointer()
                    .when(selected, |d| {
                        d.bg(p.elevated).shadow_sm().text_color(p.foreground)
                    })
                    .when(!selected, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .child(name)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.editor_mut(|e| e.kind = kind, cx)),
                    ),
            );
        }
        let schedule_body: AnyElement = match e.kind {
            Kind::Daily => {
                let mut days = h_flex().gap_1();
                for (ix, letter) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
                    let on = e.days[ix];
                    days = days.child(
                        div()
                            .id(SharedString::from(format!("automation-day-{ix}")))
                            .size(px(28.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .cursor_pointer()
                            .border_1()
                            .when(on, |d| {
                                d.bg(p.accent)
                                    .border_color(p.accent)
                                    .text_color(p.accent_foreground)
                            })
                            .when(!on, |d| {
                                d.border_color(p.border)
                                    .text_color(p.muted_foreground)
                                    .hover(|d| d.bg(p.hover))
                            })
                            .child(*letter)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.editor_mut(|e| e.days[ix] = !e.days[ix], cx)
                            })),
                    );
                }
                h_flex()
                    .gap_3()
                    .items_center()
                    .flex_wrap()
                    .child(days)
                    .child(div().text_sm().text_color(p.muted_foreground).child("at"))
                    .child(div().w(px(84.)).child(Input::new(&e.time).small()))
                    .into_any_element()
            }
            Kind::Interval => {
                let (view, unit) = (view.clone(), e.unit);
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(p.muted_foreground).child("Every"))
                    .child(div().w(px(72.)).child(Input::new(&e.every).small()))
                    .child(
                        Button::new("automation-unit")
                            .small()
                            .cursor_pointer()
                            .outline()
                            .label(unit.label())
                            .dropdown_menu(move |mut menu, _, _| {
                                for u in [Unit::Minutes, Unit::Hours, Unit::Days] {
                                    let v = view.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(u.label())
                                            .checked(u == unit)
                                            .on_click(move |_, _, cx| {
                                                v.update(cx, |this, cx| {
                                                    this.editor_mut(|e| e.unit = u, cx)
                                                })
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child("counted from the last run"),
                    )
                    .into_any_element()
            }
            Kind::Cron => v_flex()
                .gap_1()
                .child(div().w(px(220.)).child(Input::new(&e.cron).small()))
                .child(
                    div()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child("minute hour day-of-month month day-of-week, in local time. 0 9 * * 1-5: weekdays at 09:00; */30 * * * *: every half hour."),
                )
                .into_any_element(),
            Kind::Once(at) => div()
                .text_sm()
                .child(format!("Once, {}", when_text(at)))
                .into_any_element(),
        };

        // Agent, model, permissions.
        let agent_label = e
            .harness
            .as_ref()
            .map(agent_name)
            .unwrap_or_else(|| "Project's agent".into());
        let agent_btn = {
            let (view, current) = (view.clone(), e.harness.clone());
            let agents = agents.clone();
            Button::new("automation-agent")
                .small()
                .cursor_pointer()
                .outline()
                .label(agent_label)
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, name) in &agents {
                        let (v, id) = (view.clone(), *id);
                        let checked = current.as_ref().is_some_and(|c| c.as_str() == id);
                        menu = menu.item(PopupMenuItem::new(*name).checked(checked).on_click(
                            move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    let h = HarnessId(id.into());
                                    this.store
                                        .update(cx, |s, cx| s.ensure_models(h.clone(), cx));
                                    this.editor_mut(
                                        |e| {
                                            if e.harness.as_ref() != Some(&h) {
                                                e.model = None;
                                                e.effort = None;
                                            }
                                            e.harness = Some(h);
                                        },
                                        cx,
                                    )
                                })
                            },
                        ));
                    }
                    menu
                })
        };
        let model_label = match &e.model {
            Some(m) => models
                .iter()
                .find(|(id, _)| id == m)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| crate::workspace::pretty_model(m)),
            None => "Default model".into(),
        };
        let model_btn = {
            let (view, current) = (view.clone(), e.model.clone());
            Button::new("automation-model")
                .small()
                .cursor_pointer()
                .outline()
                .label(model_label)
                .dropdown_menu(move |mut menu, _, _| {
                    let v = view.clone();
                    menu = menu
                        .item(
                            PopupMenuItem::new("Agent's default")
                                .checked(current.is_none())
                                .on_click(move |_, _, cx| {
                                    v.update(cx, |this, cx| {
                                        this.editor_mut(
                                            |e| {
                                                e.model = None;
                                                e.effort = None;
                                            },
                                            cx,
                                        )
                                    })
                                }),
                        )
                        .separator();
                    for (id, name) in &models {
                        let (v, id) = (view.clone(), id.clone());
                        let checked = current.as_deref() == Some(id.as_str());
                        menu =
                            menu.item(PopupMenuItem::new(name.clone()).checked(checked).on_click(
                                move |_, _, cx| {
                                    let id = id.clone();
                                    v.update(cx, |this, cx| {
                                        this.editor_mut(
                                            |e| {
                                                e.model = Some(id);
                                                e.effort = None;
                                            },
                                            cx,
                                        )
                                    })
                                },
                            ));
                    }
                    menu
                })
        };
        let permission_btn = {
            let (view, current) = (view.clone(), e.permission);
            Button::new("automation-permission")
                .small()
                .cursor_pointer()
                .outline()
                .icon(IconName::ShieldCheck)
                .label(crate::workspace::mode_label(e.permission))
                .dropdown_menu(move |mut menu, _, _| {
                    for mode in [
                        None,
                        Some(PermissionMode::ReadOnly),
                        Some(PermissionMode::AskAll),
                        Some(PermissionMode::AcceptEdits),
                        Some(PermissionMode::Auto),
                        Some(PermissionMode::FullAccess),
                    ] {
                        let v = view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::workspace::mode_label(mode))
                                .checked(current == mode)
                                .on_click(move |_, _, cx| {
                                    v.update(cx, |this, cx| {
                                        this.editor_mut(|e| e.permission = mode, cx)
                                    })
                                }),
                        );
                    }
                    menu
                })
        };
        let asks = matches!(e.permission, Some(PermissionMode::AskAll));

        let title = if e.id.is_some() {
            "Edit automation"
        } else {
            "New automation"
        };
        let (worktree, enabled, saving) = (e.worktree, e.enabled, e.saving);
        v_flex()
            .gap_4()
            .p_5()
            .rounded(px(12.))
            .border_1()
            .border_color(p.accent.opacity(0.5))
            .bg(p.elevated)
            .shadow_sm()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .child(field("Name", Input::new(&e.name).small().into_any_element())),
                    )
                    .child(field("Project", project_btn.into_any_element())),
            )
            .child(field(
                "Prompt",
                div()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(p.border)
                    .bg(p.background)
                    .px_2()
                    .py_1p5()
                    .text_sm()
                    .child(Textarea::new(&e.prompt).appearance(false))
                    .into_any_element(),
            ))
            .child(field(
                "Schedule",
                v_flex()
                    .gap_3()
                    .child(segmented)
                    .child(schedule_body)
                    .into_any_element(),
            ))
            .child(field(
                "Runs with",
                v_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(agent_btn)
                            .child(model_btn)
                            .child(permission_btn),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if asks { p.warning } else { p.faint_foreground })
                            .child(if asks {
                                "Runs start with nobody watching: this mode stops each run at its first action until you answer."
                            } else {
                                "Runs start with nobody watching: pick permissions that let the agent do its job without asking."
                            }),
                    )
                    .into_any_element(),
            ))
            .child(
                h_flex()
                    .gap_6()
                    .flex_wrap()
                    .child(
                        Switch::new("automation-worktree")
                            .cursor_pointer()
                            .checked(worktree)
                            .label("Each run in a new worktree")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let on = *checked;
                                this.editor_mut(|e| e.worktree = on, cx)
                            })),
                    )
                    .child(
                        Switch::new("automation-enabled")
                            .cursor_pointer()
                            .checked(enabled)
                            .label("On")
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let on = *checked;
                                this.editor_mut(|e| e.enabled = on, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(p.danger)
                            .children(e.error.clone()),
                    )
                    .child(
                        Button::new("automation-cancel")
                            .small()
                            .cursor_pointer()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("automation-save")
                            .small()
                            .cursor_pointer()
                            .primary()
                            .loading(saving)
                            .label(if self.editor.as_ref().is_some_and(|e| e.id.is_some()) {
                                "Save"
                            } else {
                                "Create"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .into_any_element()
    }
}

fn agent_name(h: &HarnessId) -> String {
    crate::settings::agents()
        .iter()
        .find(|(id, _)| *id == h.as_str())
        .map(|(_, n)| (*n).to_owned())
        .unwrap_or_else(|| h.as_str().to_owned())
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// A schedule in words, a one-off with its date.
fn describe(s: &Schedule) -> String {
    match s {
        Schedule::Once { at_ms } => format!("Once, {}", when_text(*at_ms)),
        s => s.describe(),
    }
}

fn now_ms() -> i64 {
    web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A time ahead or behind, the way a person says it: "in 25 min (today
/// 09:00)", "tomorrow 09:00", "Mon 18 Nov 09:00".
fn when_text(ms: i64) -> String {
    let Some(at) = Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    let now = Local::now();
    let days = (at.date_naive() - now.date_naive()).num_days();
    let clock = at.format("%H:%M");
    let day = match days {
        0 => format!("today {clock}"),
        1 => format!("tomorrow {clock}"),
        -1 => format!("yesterday {clock}"),
        2..=6 => format!("{} {clock}", at.format("%A")),
        _ if at.year() == now.year() => format!("{} {clock}", at.format("%a %-d %b")),
        _ => format!("{} {clock}", at.format("%a %-d %b %Y")),
    };
    let mins = (ms - now_ms()) / 60_000;
    match mins {
        0 if ms >= now_ms() => format!("in under a minute ({day})"),
        1..=59 => format!("in {mins} min ({day})"),
        60..=719 => format!("in {}h {}m ({day})", mins / 60, mins % 60),
        _ => day,
    }
}

/// When a run started: "12 min ago", "today 09:00", "Mon 18 Nov 09:00".
fn started_text(ms: i64) -> String {
    let mins = (now_ms() - ms) / 60_000;
    match mins {
        ..=0 => "just now".into(),
        1..=59 => format!("{mins} min ago"),
        _ => when_text(ms),
    }
}

/// A project in the editor's project list.
#[derive(Clone)]
struct ProjectItem {
    id: ProjectId,
    name: SharedString,
    /// Its folder (`~/…`): searched too, and shown under the name.
    path: String,
    chat: bool,
}

impl SearchableListItem for ProjectItem {
    type Value = ProjectId;

    fn title(&self) -> SharedString {
        self.name.clone()
    }

    fn value(&self) -> &ProjectId {
        &self.id
    }

    fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.name.to_lowercase().contains(&q)
            || (!self.chat && self.path.to_lowercase().contains(&q))
    }

    fn render(&self, _: &mut Window, cx: &mut gpui_kit::App) -> impl IntoElement {
        let p = Palette::of(cx);
        v_flex()
            .min_w_0()
            .child(div().truncate().child(self.name.clone()))
            .when(!self.chat, |d| {
                d.child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(p.muted_foreground)
                        .child(self.path.clone()),
                )
            })
    }
}
