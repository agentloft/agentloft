//! The main window: sidebar (projects → threads) and the selected thread
//! (timeline + composer).

use crate::items::render_item;
use crate::store::{AppStore, ConnState, ThreadState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{ThreadId, ThreadStatus, TurnStatus};
use std::collections::HashMap;

pub struct WorkspaceView {
    store: Entity<AppStore>,
    composers: HashMap<ThreadId, Entity<TextareaState>>,
    observed: HashMap<ThreadId, Subscription>,
    _subs: Vec<Subscription>,
}

impl WorkspaceView {
    pub fn new(store: Entity<AppStore>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            composers: HashMap::new(),
            observed: HashMap::new(),
            _subs: vec![sub],
        }
    }

    fn composer_for(
        &mut self,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        let id = thread.read(cx).id.clone();
        if !self.observed.contains_key(&id) {
            let sub = cx.observe(thread, |_, _, cx| cx.notify());
            self.observed.insert(id.clone(), sub);
        }
        if let Some(c) = self.composers.get(&id) {
            return c.clone();
        }
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 10)
                .submit_on_enter(true)
                .placeholder("Message Claude…  (Enter to send, Shift+Enter for a newline)")
        });
        let thread_id = id.clone();
        let sub = cx.subscribe_in(&composer, window, move |this, state, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit(&thread_id, state, window, cx);
            }
        });
        self._subs.push(sub);
        self.composers.insert(id, composer.clone());
        composer
    }

    fn submit(
        &mut self,
        thread_id: &ThreadId,
        composer: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let running = self
            .store
            .read(cx)
            .threads
            .get(thread_id)
            .and_then(|t| {
                t.read(cx)
                    .model
                    .as_ref()
                    .map(|m| m.running_turn().is_some())
            })
            .unwrap_or(false);
        if running {
            return; // M0: no queueing yet (M1).
        }
        composer.update(cx, |c, cx| c.set_value("", window, cx));
        let id = thread_id.clone();
        self.store
            .update(cx, |store, cx| store.send_message(id, text, cx));
    }

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        let store = self.store.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                store.update(cx, |store, cx| {
                    store.add_project(path.to_string_lossy().into_owned(), cx)
                });
            }
        })
        .detach();
    }

    // -- sidebar ------------------------------------------------------------

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let selected = store.selected.clone();
        let mut list = v_flex().gap_3().px_2().pb_4();

        for project in &store.workspace.projects {
            let project_id = project.id.clone();
            let header = h_flex()
                .justify_between()
                .items_center()
                .px_2()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .child(project.name.to_uppercase()),
                )
                .child(
                    Button::new(SharedString::from(format!("new-{}", project.id)))
                        .label("+ New")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = project_id.clone();
                            this.store.update(cx, |s, cx| s.new_thread(id, cx));
                        })),
                );
            let mut threads = v_flex().gap_0p5();
            for thread in store.workspace.threads_for(project) {
                let id = thread.id.clone();
                let active = selected.as_ref() == Some(&thread.id);
                let dot = match thread.status {
                    ThreadStatus::Running => Some(theme.info),
                    ThreadStatus::WaitingApproval => Some(theme.warning),
                    ThreadStatus::Error => Some(theme.danger),
                    _ => None,
                };
                threads = threads.child(
                    h_flex()
                        .id(SharedString::from(format!("thread-{}", thread.id)))
                        .gap_2()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_sm()
                        .when(active, |d| d.bg(theme.list_active))
                        .hover(|d| d.bg(theme.muted))
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(dot.unwrap_or(theme.border)),
                        )
                        .child(div().flex_1().truncate().child(thread.title.clone()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = id.clone();
                            this.store.update(cx, |s, cx| s.select_thread(id, cx));
                        })),
                );
            }
            list = list.child(v_flex().gap_1().child(header).child(threads));
        }

        let status: SharedString = match &store.conn {
            ConnState::Connecting => "Connecting to agentloftd…".into(),
            ConnState::Connected => "Connected".into(),
            ConnState::Disconnected(why) => format!("Disconnected: {why}").into(),
        };
        let error = store.last_error.clone();

        v_flex()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_4()
                    .pt_8()
                    .pb_3()
                    .child(div().font_weight(FontWeight::BOLD).child("Agentloft"))
                    .child(
                        Button::new("add-project")
                            .label("Add project")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| this.add_project(cx))),
                    ),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                v_flex()
                    .px_4()
                    .py_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status)
                    .when_some(error, |d, e| {
                        d.child(div().text_color(theme.danger).child(e))
                    }),
            )
    }

    // -- thread pane --------------------------------------------------------

    fn render_thread(
        &mut self,
        thread: Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let composer = self.composer_for(&thread, window, cx);
        let state = thread.read(cx);
        let Some(model) = state.model.as_ref() else {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Loading thread…")
                .into_any_element();
        };
        let thread_id = model.thread.id.clone();
        let running = model.running_turn().is_some();
        let waiting = model.open_requests().next().is_some();
        let summary = turn_summary(model);
        let title = model.thread.title.clone();
        let sub = format!(
            "{} · {}{}",
            model.thread.harness,
            model
                .thread
                .model
                .clone()
                .unwrap_or_else(|| "default model".into()),
            model
                .thread
                .permission_mode
                .map(|m| format!(" · {m:?}"))
                .unwrap_or_default()
        );
        let scroller_state = state.scroller.clone();

        let rows = {
            let thread = thread.clone();
            let store = self.store.clone();
            MessageScroller::new("timeline", scroller_state, move |ix, window, cx| {
                render_item(&thread, &store, ix, window, cx)
            })
            .flex_1()
        };

        let action = if running {
            let id = thread_id.clone();
            Button::new("stop")
                .label("Stop")
                .danger()
                .small()
                .on_click(cx.listener(move |this, _, _, cx| {
                    let id = id.clone();
                    this.store.update(cx, |s, cx| s.interrupt(id, cx));
                }))
        } else {
            let id = thread_id.clone();
            let composer = composer.clone();
            Button::new("send")
                .label("Send")
                .primary()
                .small()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.submit(&id, &composer, window, cx);
                }))
        };

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                v_flex()
                    .px_6()
                    .pt_6()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(
                        h_flex()
                            .gap_3()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(sub)
                            .when_some(summary, |d, s| d.child(s)),
                    ),
            )
            .child(div().flex_1().min_h_0().px_4().child(rows))
            .child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .when(waiting, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("Waiting for your approval above"),
                        )
                    })
                    .child(Textarea::new(&composer))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .when(running, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Claude is working…"),
                                )
                            })
                            .child(action),
                    ),
            )
            .into_any_element()
    }
}

fn turn_summary(model: &loft_client::model::ThreadModel) -> Option<String> {
    let turn = model.last_turn()?;
    let status = match turn.status {
        TurnStatus::Running => return Some("running…".into()),
        TurnStatus::Completed => "completed",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
    };
    let mut parts = vec![format!("last turn {status}")];
    if let Some(u) = &turn.usage {
        if let Some(cost) = u.cost_usd {
            parts.push(format!("${cost:.4}"));
        }
        if let Some(ms) = u.duration_ms {
            parts.push(format!("{:.1}s", ms as f64 / 1000.0));
        }
        parts.push(format!(
            "{} in / {} out",
            u.input_tokens + u.cache_read_tokens + u.cache_write_tokens,
            u.output_tokens
        ));
    }
    Some(parts.join(" · "))
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let selected = self.store.read(cx).selected_thread();
        let has_projects = !self.store.read(cx).workspace.projects.is_empty();
        let main = match selected {
            Some(thread) => self.render_thread(thread, window, cx).into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().text_lg().child("Agentloft"))
                .child(if has_projects {
                    "Pick a thread, or start one with “+ New” next to a project."
                } else {
                    "Add a project folder to get started."
                })
                .into_any_element(),
        };
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(main)
    }
}
