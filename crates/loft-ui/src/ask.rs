//! What an agent asks of the user beyond allow/deny: questions to answer
//! (Claude's `AskUserQuestion`, Codex's `requestUserInput`) and a plan to
//! approve (Claude's `ExitPlanMode`). The open request shows as a form above
//! the composer; once answered, the timeline keeps a record of it.

use crate::store::{AppStore, ThreadState};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{
    DecisionKind, Item, ItemBody, ItemStatus, Question, QuestionAnswer, RequestKind, RequestReply,
    RequestStatus, RuntimeRequest,
};

/// Claude's tool names for these (their tool calls get a record in the
/// timeline instead of a line in a tool group).
pub(crate) const QUESTION_TOOL: &str = "AskUserQuestion";
pub(crate) const PLAN_TOOL: &str = "ExitPlanMode";

/// A request answered through a form rather than approval buttons.
pub(crate) fn is_form(req: &RuntimeRequest) -> bool {
    matches!(req.kind, RequestKind::Question | RequestKind::Plan)
}

/// A tool call shown as a question/plan record rather than a tool line.
pub(crate) fn is_record(item: &Item) -> bool {
    matches!(&item.body, ItemBody::ToolCall { tool, .. }
        if tool == QUESTION_TOOL || tool == PLAN_TOOL)
}

/// The open form for one question or plan request.
pub(crate) struct AskForm {
    pub(crate) request: RuntimeRequest,
    store: Entity<AppStore>,
    agent: &'static str,
    questions: Vec<Question>,
    /// The question on show.
    current: usize,
    /// Options picked, per question.
    picked: Vec<Vec<usize>>,
    /// The typed answer, per question.
    other: Vec<Entity<InputState>>,
    /// Plan: what to change.
    feedback: Entity<InputState>,
    /// Plan: shown at full height.
    expanded: bool,
    /// Answered; waiting for the request to close.
    sent: bool,
    /// Keys for the form itself: 1–9 pick, ↩ next/submit, ←/→ questions.
    focus: gpui_kit::FocusHandle,
    _subs: Vec<Subscription>,
}

impl AskForm {
    pub(crate) fn new(
        request: RuntimeRequest,
        store: Entity<AppStore>,
        agent: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let questions = request.questions();
        let mut subs = Vec::new();
        let other: Vec<Entity<InputState>> = questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let placeholder = if q.options.is_empty() {
                    "Type your answer…"
                } else {
                    "Something else…"
                };
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .masked(q.secret)
                });
                subs.push(cx.subscribe_in(
                    &input,
                    window,
                    move |this, input, event, window, cx| {
                        match event {
                            // A typed answer replaces a single pick.
                            InputEvent::Change => {
                                let typed = !input.read(cx).value().trim().is_empty();
                                if typed && !this.questions[ix].multi_select {
                                    this.picked[ix].clear();
                                }
                                cx.notify();
                            }
                            InputEvent::PressEnter { .. } => this.next_or_submit(window, cx),
                            _ => {}
                        }
                    },
                ));
                input
            })
            .collect();
        let feedback = cx.new(|cx| {
            InputState::new(window, cx).placeholder(format!("Tell {agent} what to change…"))
        });
        subs.push(
            cx.subscribe_in(&feedback, window, |this, input, event, _, cx| match event {
                InputEvent::Change => cx.notify(),
                // Enter sends the feedback typed.
                InputEvent::PressEnter { .. } if !input.read(cx).value().trim().is_empty() => {
                    this.decide(DecisionKind::Deny, cx);
                }
                _ => {}
            }),
        );
        // A question takes the keys (1–9, ↩) as it opens.
        let focus = cx.focus_handle();
        if request.kind == RequestKind::Question {
            window.focus(&focus, cx);
        }
        Self {
            focus,
            picked: vec![Vec::new(); questions.len()],
            questions,
            request,
            store,
            agent,
            current: 0,
            other,
            feedback,
            expanded: false,
            sent: false,
            _subs: subs,
        }
    }

    fn typed(&self, ix: usize, cx: &App) -> Option<String> {
        let text = self.other.get(ix)?.read(cx).value().trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    fn answered(&self, ix: usize, cx: &App) -> bool {
        !self.picked[ix].is_empty() || self.typed(ix, cx).is_some()
    }

    fn all_answered(&self, cx: &App) -> bool {
        (0..self.questions.len()).all(|ix| self.answered(ix, cx))
    }

    fn answers(&self, cx: &App) -> Vec<QuestionAnswer> {
        self.questions
            .iter()
            .enumerate()
            .filter(|(ix, _)| self.answered(*ix, cx))
            .map(|(ix, q)| {
                let mut picked = self.picked[ix].clone();
                picked.sort_unstable();
                let mut answers: Vec<String> = picked
                    .iter()
                    .filter_map(|o| q.options.get(*o).map(|o| o.label.clone()))
                    .collect();
                answers.extend(self.typed(ix, cx));
                QuestionAnswer {
                    id: q.id.clone(),
                    answers,
                }
            })
            .collect()
    }

    fn pick(&mut self, ix: usize, option: usize, window: &mut Window, cx: &mut Context<Self>) {
        let multi = self.questions[ix].multi_select;
        let picked = &mut self.picked[ix];
        if multi {
            match picked.iter().position(|o| *o == option) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option),
            }
        } else {
            *picked = vec![option];
            // A pick replaces what was typed.
            self.other[ix].update(cx, |s, cx| s.set_value("", window, cx));
            // On to the next question still unanswered.
            if let Some(next) = (0..self.questions.len())
                .map(|k| (ix + 1 + k) % self.questions.len())
                .find(|k| !self.answered(*k, cx))
            {
                self.current = next;
            }
        }
        cx.notify();
    }

    fn next_or_submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.all_answered(cx) {
            self.submit(cx);
        } else if let Some(next) = (0..self.questions.len())
            .map(|k| (self.current + 1 + k) % self.questions.len())
            .find(|k| !self.answered(*k, cx))
        {
            self.current = next;
            cx.notify();
        }
    }

    /// Send the answers (⌘↩); nothing happens until every question has one.
    pub(crate) fn submit(&mut self, cx: &mut Context<Self>) {
        if self.sent || !self.all_answered(cx) {
            return;
        }
        let reply = RequestReply {
            answers: self.answers(cx),
            message: None,
        };
        self.send(DecisionKind::AllowOnce, Some(reply), cx);
    }

    /// Answer with `decision`: a plan's approval, or "keep planning" with
    /// the feedback typed; a question's skip or stop.
    pub(crate) fn decide(&mut self, decision: DecisionKind, cx: &mut Context<Self>) {
        if self.sent || !self.request.options.contains(&decision) {
            return;
        }
        if self.request.kind == RequestKind::Question && decision == DecisionKind::AllowOnce {
            return self.submit(cx);
        }
        let message = self.feedback.read(cx).value().trim().to_owned();
        let reply = (self.request.kind == RequestKind::Plan
            && decision == DecisionKind::Deny
            && !message.is_empty())
        .then(|| RequestReply {
            answers: vec![],
            message: Some(message),
        });
        self.send(decision, reply, cx);
    }

    fn send(
        &mut self,
        decision: DecisionKind,
        reply: Option<RequestReply>,
        cx: &mut Context<Self>,
    ) {
        self.sent = true;
        let (thread_id, request_id) = (self.request.thread_id.clone(), self.request.id.clone());
        self.store.update(cx, |s, cx| {
            s.respond_with(thread_id, request_id, decision, reply, cx)
        });
        cx.notify();
    }

    /// Keys while the form itself has focus (not one of its fields).
    fn on_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.request.kind != RequestKind::Question || self.questions.is_empty() {
            return false;
        }
        let ix = self.current.min(self.questions.len() - 1);
        match key {
            "enter" => self.next_or_submit(window, cx),
            "left" if ix > 0 => self.current = ix - 1,
            "right" if ix + 1 < self.questions.len() => self.current = ix + 1,
            "escape" => self.decide(DecisionKind::Deny, cx),
            k => {
                let Some(n) = k.parse::<usize>().ok().filter(|n| *n >= 1) else {
                    return false;
                };
                let q = &self.questions[ix];
                if n <= q.options.len() {
                    self.pick(ix, n - 1, window, cx);
                } else if n == q.options.len() + 1 && q.allow_other {
                    self.other[ix].update(cx, |s, cx| s.focus(window, cx));
                } else {
                    return false;
                }
            }
        }
        cx.notify();
        true
    }

    fn render_question(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.questions.len();
        if count == 0 {
            return div().into_any_element();
        }
        let ix = self.current.min(count - 1);
        let id = self.request.id.clone();
        let q = self.questions[ix].clone();

        // Top line: who asks (or the questions, as tabs) and where we are.
        let mut top = h_flex().gap_1().items_center().flex_wrap().min_w_0().child(
            div()
                .mr_1()
                .text_color(p.accent)
                .child(Icon::new(IconName::MessageCircleQuestionMark).xsmall()),
        );
        if count == 1 {
            top = top.child(
                div()
                    .text_xs()
                    .text_color(p.muted_foreground)
                    .child(format!("{} has a question", self.agent)),
            );
        } else {
            for (k, question) in self.questions.iter().enumerate() {
                let active = k == ix;
                let done = self.answered(k, cx);
                let label = if question.header.is_empty() {
                    format!("Question {}", k + 1)
                } else {
                    question.header.clone()
                };
                top = top.child(
                    h_flex()
                        .id(SharedString::from(format!("ask-tab-{id}-{k}")))
                        .gap_1()
                        .px_2()
                        .py(px(2.))
                        .rounded(px(6.))
                        .text_xs()
                        .cursor_pointer()
                        .when(active, |d| d.bg(p.muted).text_color(p.foreground))
                        .when(!active, |d| {
                            d.text_color(p.muted_foreground)
                                .hover(|d| d.text_color(p.foreground))
                        })
                        .when(done && !active, |d| {
                            d.child(
                                div()
                                    .text_color(p.success)
                                    .child(Icon::new(IconName::Check).xsmall()),
                            )
                        })
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.current = k;
                            cx.notify();
                        })),
                );
            }
        }
        let top = h_flex()
            .justify_between()
            .items_center()
            .gap_2()
            .child(top)
            .when(count > 1, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(format!("{} of {count}", ix + 1)),
                )
            });

        // The question wraps (agents write long ones); "Pick any" under it.
        let question = v_flex()
            .px_1()
            .gap_0p5()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.foreground)
                    .child(q.question.clone()),
            )
            .when(q.multi_select, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child("Pick any"),
                )
            });

        let badge = |text: AnyElement, on: bool| {
            div()
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .text_xs()
                .when(on, |d| d.bg(p.accent).text_color(p.accent_foreground))
                .when(!on, |d| {
                    d.border_1()
                        .border_color(p.border)
                        .text_color(p.muted_foreground)
                })
                .child(text)
        };
        let mut options = v_flex().gap_0p5();
        for (o, option) in q.options.iter().enumerate() {
            let on = self.picked[ix].contains(&o);
            // "(Recommended)" reads better as a tag.
            let (label, recommended) = match option.label.strip_suffix("(Recommended)") {
                Some(rest) => (rest.trim_end().to_owned(), true),
                None => (option.label.clone(), false),
            };
            let mark = if on && q.multi_select {
                Icon::new(IconName::Check).xsmall().into_any_element()
            } else {
                format!("{}", o + 1).into_any_element()
            };
            options = options.child(
                h_flex()
                    .id(SharedString::from(format!("ask-opt-{id}-{ix}-{o}")))
                    .gap_2p5()
                    .items_start()
                    .px_2()
                    .py_1p5()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(p.accent_soft))
                    .when(!on, |d| d.hover(|d| d.bg(p.hover)))
                    .child(badge(mark, on))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .pt(px(1.))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .flex_shrink_1()
                                            .min_w_0()
                                            .text_sm()
                                            .text_color(p.foreground)
                                            .child(label),
                                    )
                                    .when(recommended, |d| {
                                        d.child(
                                            div()
                                                .px_1p5()
                                                .rounded(px(4.))
                                                .bg(p.accent_soft)
                                                .text_color(p.accent)
                                                .text_xs()
                                                .child("Recommended"),
                                        )
                                    }),
                            )
                            .when(!option.description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(p.muted_foreground)
                                        .child(option.description.clone()),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.pick(ix, o, window, cx))),
            );
        }
        if q.allow_other {
            let typed = self.typed(ix, cx).is_some();
            let n = q.options.len() + 1;
            options = options.child(
                h_flex()
                    .gap_2p5()
                    .items_center()
                    .pl_2()
                    .pr_1()
                    .rounded(px(8.))
                    .when(typed, |d| d.bg(p.accent_soft))
                    .child(badge(format!("{n}").into_any_element(), typed))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.other[ix]).appearance(false).small()),
                    ),
            );
        }

        // Skip on the left; back and next/submit on the right.
        let ready = self.all_answered(cx);
        let last = !(0..count).any(|k| k != ix && !self.answered(k, cx));
        let mut right = h_flex().gap_1().items_center();
        if count > 1 && ix > 0 {
            right = right.child(
                Button::new(SharedString::from(format!("{id}-back")))
                    .label("Back")
                    .ghost()
                    .small()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.current = this.current.saturating_sub(1);
                        cx.notify();
                    })),
            );
        }
        let primary = if ready || last {
            Button::new(SharedString::from(format!("{id}-submit")))
                .label(format!(
                    "Submit  {}",
                    crate::keymap::display("secondary-enter")
                ))
                .primary()
                .small()
                .disabled(!ready || self.sent)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
        } else {
            Button::new(SharedString::from(format!("{id}-next")))
                .label("Next")
                .primary()
                .small()
                .disabled(!self.answered(ix, cx))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| this.next_or_submit(window, cx)))
        };
        right = right.child(primary);
        let footer = h_flex()
            .pt_1()
            .justify_between()
            .items_center()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(self.request.options.contains(&DecisionKind::Deny), |d| {
                        d.child(
                            Button::new(SharedString::from(format!("{id}-skip")))
                                .label("Skip")
                                .ghost()
                                .small()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.decide(DecisionKind::Deny, cx)
                                })),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child("1–9 to pick · ↩ to continue"),
                    ),
            )
            .child(right);

        shell(p, "ask", &id, cx)
            .gap_2()
            .child(top)
            .child(question)
            .child(options)
            .child(footer)
            .into_any_element()
    }

    fn render_plan(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = self.request.id.clone();
        let plan = self.request.plan().unwrap_or_default().to_owned();
        let expanded = self.expanded;
        let top = h_flex()
            .justify_between()
            .items_center()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_color(p.accent)
                            .child(Icon::new(IconName::ListChecks).xsmall()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .child(format!("{} has a plan", self.agent)),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-expand")))
                    .icon(if expanded {
                        IconName::ChevronsDownUp
                    } else {
                        IconName::ChevronsUpDown
                    })
                    .ghost()
                    .xsmall()
                    .cursor_pointer()
                    .tooltip(if expanded { "Collapse" } else { "Expand" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            );
        let body = div()
            .id(SharedString::from(format!("plan-{id}")))
            .px_1()
            .when(!expanded, |d| d.max_h(px(300.)))
            .overflow_y_scroll()
            .child(
                TextView::markdown(SharedString::from(format!("plan-md-{id}")), plan)
                    .selectable(true),
            );

        let feedback = !self.feedback.read(cx).value().trim().is_empty();
        let mut options = self.request.options.clone();
        // Keep planning, then the approvals, the primary last (rightmost).
        options.sort_by_key(|d| match d {
            DecisionKind::DenyAndStop => 0,
            DecisionKind::Deny => 1,
            DecisionKind::AllowSession => 2,
            DecisionKind::AllowOnce => 3,
        });
        let mut buttons = h_flex().gap_1().items_center();
        for decision in options {
            let label = match decision {
                DecisionKind::AllowOnce => {
                    format!("Approve  {}", crate::keymap::display("secondary-enter"))
                }
                DecisionKind::AllowSession => "Approve & accept edits".to_owned(),
                DecisionKind::Deny if feedback => "Send feedback".to_owned(),
                DecisionKind::Deny => "Keep planning".to_owned(),
                DecisionKind::DenyAndStop => "Discard".to_owned(),
            };
            let button = Button::new(SharedString::from(format!("{id}-{decision:?}")))
                .label(label)
                .small()
                .disabled(self.sent)
                .cursor_pointer();
            let button = match decision {
                DecisionKind::AllowOnce => button.primary(),
                DecisionKind::Deny if feedback => button.outline(),
                _ => button.ghost(),
            };
            buttons = buttons.child(
                button.on_click(cx.listener(move |this, _, _, cx| this.decide(decision, cx))),
            );
        }
        let footer = h_flex()
            .gap_2()
            .items_center()
            .pt_2()
            .border_t_1()
            .border_color(p.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.feedback).appearance(false).small()),
            )
            .child(buttons);

        shell(p, "plan", &id, cx)
            .gap_2()
            .child(top)
            .child(body)
            .child(footer)
            .into_any_element()
    }
}

impl Render for AskForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let body = match self.request.kind {
            RequestKind::Plan => self.render_plan(&p, cx),
            _ => self.render_question(&p, cx),
        };
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, e: &gpui_kit::KeyDownEvent, window, cx| {
                // Only when the form itself is focused, not a field in it.
                let m = &e.keystroke.modifiers;
                if this.focus.is_focused(window)
                    && !(m.platform || m.control || m.alt)
                    && this.on_key(&e.keystroke.key, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .child(body)
    }
}

/// The card around a form, in the composer's style.
fn shell(
    p: &Palette,
    kind: &str,
    id: &loft_protocol::RequestId,
    cx: &mut Context<AskForm>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    v_flex()
        .id(SharedString::from(format!("{kind}-card-{id}")))
        .p_2p5()
        .rounded(px(16.))
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .shadow_md()
        // A click on the card (not a field) gives it the keys.
        .on_click(cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
}

/// The timeline's record of a question or plan tool call: what was asked
/// and answered, or the plan and what became of it.
pub(crate) fn render_record(
    thread: &Entity<ThreadState>,
    state: &ThreadState,
    item: &Item,
    p: &Palette,
) -> AnyElement {
    let ItemBody::ToolCall {
        tool,
        input,
        output,
        ..
    } = &item.body
    else {
        return div().into_any_element();
    };
    let request = state
        .model
        .as_ref()
        .and_then(|m| m.request_for_item(&item.id));
    let open = request.is_some_and(|r| r.status == RequestStatus::Open);
    let declined = item.status == ItemStatus::Declined
        || request.is_some_and(|r| {
            matches!(
                r.decision,
                Some(DecisionKind::Deny | DecisionKind::DenyAndStop)
            )
        });
    let fold_key = loft_protocol::ItemId(format!("fold:{}", item.id.0));
    let unfolded = state.is_expanded(&fold_key, false);
    let toggle = {
        let thread = thread.clone();
        let key = fold_key.clone();
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
            thread.update(cx, |t, cx| t.toggle(key.clone(), false, cx));
        }
    };

    if tool == PLAN_TOOL {
        let plan = input
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let (status, color) = if open {
            ("Waiting for your approval", p.warning)
        } else if declined {
            ("Sent back", p.muted_foreground)
        } else if item.status == ItemStatus::InProgress && request.is_none() {
            ("Proposed", p.muted_foreground)
        } else {
            ("Approved", p.success)
        };
        let feedback = request
            .and_then(|r| r.reply.as_ref())
            .and_then(|r| r.message.clone());
        return v_flex()
            .gap_2()
            .p_3()
            .rounded(px(12.))
            .border_1()
            .border_color(p.border)
            .bg(p.elevated)
            .child(
                h_flex()
                    .id(SharedString::from(format!("plan-rec-{}", item.id)))
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .text_color(p.accent)
                            .child(Icon::new(IconName::ListChecks).small()),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_sm()
                            .child("Plan"),
                    )
                    .child(div().text_xs().text_color(color).child(status))
                    .child(div().flex_1())
                    .child(
                        div().text_color(p.muted_foreground).child(
                            Icon::new(if unfolded {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall(),
                        ),
                    )
                    .on_click(toggle),
            )
            .when(!unfolded, |d| {
                d.children(crate::items::first_line(&plan).map(|line| {
                    div()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .truncate()
                        .child(line)
                }))
            })
            .when(unfolded, |d| {
                d.child(
                    TextView::markdown(
                        SharedString::from(format!("plan-rec-md-{}", item.id)),
                        plan,
                    )
                    .selectable(true),
                )
            })
            .when_some(feedback, |d, f| {
                d.child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(Icon::new(IconName::CornerDownRight).xsmall())
                        .child(format!("You said: {f}")),
                )
            })
            .into_any_element();
    }

    // Questions: each with what was answered.
    let questions: Vec<Question> = request
        .map(|r| r.questions())
        .filter(|q| !q.is_empty())
        .unwrap_or_else(|| {
            input
                .get("questions")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|q| {
                    let question = q.get("question")?.as_str()?.to_owned();
                    Some(Question {
                        id: question.clone(),
                        question,
                        ..Default::default()
                    })
                })
                .collect()
        });
    let answers = request.and_then(|r| r.reply.as_ref()).map(|r| &r.answers);
    let status = if open {
        "Waiting for your answer"
    } else if declined {
        "Not answered"
    } else {
        "Answered"
    };
    let mut col = v_flex().gap_1p5().child(
        h_flex()
            .gap_2()
            .items_center()
            .text_sm()
            .text_color(p.muted_foreground)
            .child(Icon::new(IconName::MessageCircleQuestionMark).small())
            .child(if questions.len() > 1 {
                format!("Asked {} questions", questions.len())
            } else {
                "Asked a question".to_owned()
            })
            .child(div().text_xs().text_color(p.faint_foreground).child(status)),
    );
    let mut list = v_flex()
        .gap_1()
        .pl(px(7.))
        .ml(px(7.))
        .border_l_1()
        .border_color(p.border);
    for q in &questions {
        let answer = answers
            .and_then(|a| a.iter().find(|a| a.id == q.id))
            .map(|a| a.answers.join(", "))
            .or_else(|| {
                // Older records: the tool's own result says it.
                output
                    .as_deref()
                    .and_then(|o| answer_in_output(o, &q.question))
            });
        list = list.child(
            v_flex()
                .gap_0p5()
                .px_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(p.foreground)
                        .child(q.question.clone()),
                )
                .when_some(answer, |d, a| {
                    d.child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .text_color(p.accent)
                            .child(Icon::new(IconName::CornerDownRight).xsmall())
                            .child(a),
                    )
                }),
        );
    }
    col = col.child(list);
    col.into_any_element()
}

/// The answer to `question` in Claude's `AskUserQuestion` result text
/// (`"question"="answer"`).
fn answer_in_output(output: &str, question: &str) -> Option<String> {
    let key = format!("\"{question}\"=\"");
    let at = output.find(&key)? + key.len();
    let rest = &output[at..];
    Some(rest[..rest.find('"')?].to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn answers_are_read_from_claude_results() {
        let out = r#"User has answered your questions: "Which DB?"="SQLite", "Extras?"="Tests, Docs". You can now continue."#;
        assert_eq!(
            super::answer_in_output(out, "Extras?").as_deref(),
            Some("Tests, Docs")
        );
        assert_eq!(super::answer_in_output(out, "Nope?"), None);
    }
}
