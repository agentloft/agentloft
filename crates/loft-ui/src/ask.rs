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
        Self {
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

    fn render_question(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.questions.len();
        let ix = self.current.min(count.saturating_sub(1));
        let headline = if count > 1 {
            format!("{} has {count} questions", self.agent)
        } else {
            format!("{} has a question", self.agent)
        };
        let mut card = shell(p, IconName::MessageCircleQuestionMark, headline, p.accent);
        if count == 0 {
            return card.into_any_element();
        }

        // Tabs, when there is more than one question.
        if count > 1 {
            let mut tabs = h_flex().gap_1().flex_wrap();
            for (k, q) in self.questions.iter().enumerate() {
                let active = k == ix;
                let done = self.answered(k, cx);
                let label = if q.header.is_empty() {
                    format!("Question {}", k + 1)
                } else {
                    q.header.clone()
                };
                tabs = tabs.child(
                    h_flex()
                        .id(SharedString::from(format!(
                            "ask-tab-{}-{k}",
                            self.request.id
                        )))
                        .gap_1()
                        .px_2()
                        .py(px(3.))
                        .rounded(px(999.))
                        .text_xs()
                        .cursor_pointer()
                        .border_1()
                        .border_color(if active { p.accent } else { p.border })
                        .when(active, |d| d.bg(p.accent_soft).text_color(p.foreground))
                        .when(!active, |d| {
                            d.text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        })
                        .when(done, |d| {
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
            card = card.child(tabs);
        }

        let q = self.questions[ix].clone();
        card = card.child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.foreground)
                .child(q.question.clone()),
        );
        if q.multi_select {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("Pick any that apply"),
            );
        }

        let mut options = v_flex().gap_1p5();
        for (o, option) in q.options.iter().enumerate() {
            let on = self.picked[ix].contains(&o);
            let mark = match (q.multi_select, on) {
                (true, true) => IconName::SquareCheck,
                (true, false) => IconName::Square,
                (false, true) => IconName::CircleDot,
                (false, false) => IconName::Circle,
            };
            options = options.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "ask-opt-{}-{ix}-{o}",
                        self.request.id
                    )))
                    .gap_2p5()
                    .items_start()
                    .px_3()
                    .py_2()
                    .rounded(px(10.))
                    .border_1()
                    .cursor_pointer()
                    .border_color(if on { p.accent } else { p.border })
                    .bg(if on { p.accent_soft } else { p.background })
                    .when(!on, |d| d.hover(|d| d.bg(p.hover)))
                    .child(
                        div()
                            .pt(px(2.))
                            .text_color(if on { p.accent } else { p.faint_foreground })
                            .child(Icon::new(mark).small()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(p.foreground)
                                    .child(option.label.clone()),
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
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.faint_foreground)
                            .child(format!("{}", o + 1)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.pick(ix, o, window, cx))),
            );
        }
        if q.allow_other {
            let typed = self.typed(ix, cx).is_some();
            options = options.child(
                div()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(if typed { p.accent } else { p.border })
                    .bg(p.background)
                    .px_1()
                    .child(Input::new(&self.other[ix]).appearance(false).small()),
            );
        }
        card = card.child(options);

        // Skip / stop on the left; back, next or submit on the right.
        let id = self.request.id.clone();
        let ready = self.all_answered(cx);
        let last = !(0..count).any(|k| k != ix && !self.answered(k, cx));
        let submit_hint = crate::keymap::display("secondary-enter");
        let mut left = h_flex().gap_1();
        if self.request.options.contains(&DecisionKind::DenyAndStop) {
            left = left.child(
                Button::new(SharedString::from(format!("{id}-stop")))
                    .label("Stop")
                    .ghost()
                    .danger()
                    .small()
                    .cursor_pointer()
                    .on_click(
                        cx.listener(|this, _, _, cx| this.decide(DecisionKind::DenyAndStop, cx)),
                    ),
            );
        }
        if self.request.options.contains(&DecisionKind::Deny) {
            left = left.child(
                Button::new(SharedString::from(format!("{id}-skip")))
                    .label("Skip")
                    .ghost()
                    .small()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.decide(DecisionKind::Deny, cx))),
            );
        }
        let mut right = h_flex().gap_1();
        if count > 1 && ix > 0 {
            right = right.child(
                Button::new(SharedString::from(format!("{id}-back")))
                    .label("Back")
                    .outline()
                    .small()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.current = this.current.saturating_sub(1);
                        cx.notify();
                    })),
            );
        }
        let primary = if ready || last || count == 1 {
            Button::new(SharedString::from(format!("{id}-submit")))
                .label(format!("Submit  {submit_hint}"))
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
        card.child(
            h_flex()
                .pt_1()
                .justify_between()
                .items_center()
                .child(left)
                .child(right),
        )
        .into_any_element()
    }

    fn render_plan(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = self.request.id.clone();
        let plan = self.request.plan().unwrap_or_default().to_owned();
        let expanded = self.expanded;
        let card = shell(
            p,
            IconName::ListChecks,
            format!("{} has a plan", self.agent),
            p.accent,
        )
        .child(
            div()
                .text_xs()
                .text_color(p.muted_foreground)
                .child("Approve it to start, or say what to change and it keeps planning."),
        )
        .child(
            div()
                .relative()
                .rounded(px(10.))
                .border_1()
                .border_color(p.border)
                .bg(p.background)
                .child(
                    div()
                        .id(SharedString::from(format!("plan-{id}")))
                        .px_4()
                        .py_3()
                        .when(!expanded, |d| d.max_h(px(320.)))
                        .overflow_y_scroll()
                        .child(
                            TextView::markdown(SharedString::from(format!("plan-md-{id}")), plan)
                                .selectable(true),
                        ),
                )
                .child(
                    div().absolute().top(px(6.)).right(px(6.)).child(
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
                    ),
                ),
        );
        let feedback = !self.feedback.read(cx).value().trim().is_empty();
        let hint = |keys: &str| crate::keymap::display(keys);
        let mut buttons = h_flex().gap_1p5().flex_wrap().items_center();
        // Approve first (the primary), then the rest.
        let mut options = self.request.options.clone();
        options.sort_by_key(|d| match d {
            DecisionKind::AllowOnce => 0,
            DecisionKind::AllowSession => 1,
            DecisionKind::Deny => 2,
            DecisionKind::DenyAndStop => 3,
        });
        for decision in options {
            let (label, keys) = match decision {
                DecisionKind::AllowOnce => ("Approve", "secondary-enter"),
                DecisionKind::AllowSession => ("Approve & accept edits", "secondary-shift-enter"),
                DecisionKind::Deny if feedback => ("Send feedback", "secondary-shift-backspace"),
                DecisionKind::Deny => ("Keep planning", "secondary-shift-backspace"),
                DecisionKind::DenyAndStop => ("Stop", ""),
            };
            let keys = hint(keys);
            let label = if keys.is_empty() {
                label.to_owned()
            } else {
                format!("{label}  {keys}")
            };
            let button = Button::new(SharedString::from(format!("{id}-{decision:?}")))
                .label(label)
                .small()
                .disabled(self.sent)
                .cursor_pointer();
            let button = match decision {
                DecisionKind::AllowOnce => button.primary(),
                DecisionKind::AllowSession => button.outline(),
                DecisionKind::Deny if feedback => button.outline(),
                DecisionKind::Deny => button.ghost(),
                DecisionKind::DenyAndStop => button.ghost().danger(),
            };
            buttons = buttons.child(
                button.on_click(cx.listener(move |this, _, _, cx| this.decide(decision, cx))),
            );
        }
        card.child(
            div()
                .rounded(px(10.))
                .border_1()
                .border_color(if feedback { p.accent } else { p.border })
                .bg(p.background)
                .px_1()
                .child(Input::new(&self.feedback).appearance(false).small()),
        )
        .child(buttons)
        .into_any_element()
    }
}

impl Render for AskForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        match self.request.kind {
            RequestKind::Plan => self.render_plan(&p, cx),
            _ => self.render_question(&p, cx),
        }
    }
}

/// The card around a form: an accent edge, an icon and a headline.
fn shell(p: &Palette, icon: IconName, headline: String, tint: gpui_kit::Hsla) -> gpui_kit::Div {
    v_flex()
        .p_3()
        .gap_2p5()
        .rounded(px(14.))
        .border_1()
        .border_color(tint.opacity(0.4))
        .bg(p.elevated)
        .shadow_md()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_color(tint).child(Icon::new(icon).small()))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.foreground)
                        .child(headline),
                ),
        )
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
