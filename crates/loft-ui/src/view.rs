//! Views an agent draws in the chat (`show_ui`, ADR-0018): a tree of blocks
//! read by `loft-view`, drawn here with the app's own components, so a view
//! looks and behaves like the rest of Agentloft on the desktop and in a
//! browser alike.
//!
//! A view lives in the timeline where its tool call is. Its state (slider
//! positions, what was typed, which tab is open) belongs to this window and
//! starts over when the thread is loaded again. The only way out of a view
//! is a button: it can say something to the agent as the user, copy text, or
//! open a link or a file.

use crate::store::{AppStore, ThreadState};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::chart::{AreaChart, BarChart, LineChart, PieChart};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
    rgb,
};
use loft_protocol::{Item, ItemBody, ItemId, ItemStatus, ThreadId};
use loft_view::expr::{self, Format, Scope, Template};
use loft_view::{
    Action, Align, Block, ButtonStyle, Chart, ChartKind, Choice, Column, ColumnAlign, Data, Gap,
    Justify, Kind, ListItem, Size, Step, StepStatus, Tab, Tone, View, Weight,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::rc::Rc;

/// Rows a table shows before "Show all".
const TABLE_PREVIEW: usize = 8;
/// Slices a pie shows before the rest fold into "Other".
const PIE_SLICES: usize = 6;

/// Whether a timeline item is a view: a `show_ui` call that went through
/// (or is going through). One that was turned down stays an ordinary failed
/// tool line, so what the agent got wrong can be read.
pub(crate) fn is_view(item: &Item) -> bool {
    matches!(&item.body, ItemBody::ToolCall { tool, is_error, .. }
        if loft_view::is_tool(tool)
            && !is_error
            && matches!(item.status, ItemStatus::InProgress | ItemStatus::Completed))
}

/// The row of a view item.
pub(crate) fn render(state: &ThreadState, item: &Item, p: &Palette) -> AnyElement {
    if let Some(card) = state.views.get(&item.id) {
        return card.clone().into_any_element();
    }
    // Not drawable: still arriving, or (from an older build, say) not a
    // spec this build reads.
    let waiting = item.status == ItemStatus::InProgress;
    h_flex()
        .gap_2()
        .text_sm()
        .text_color(p.muted_foreground)
        .when(waiting, |d| {
            d.child(Spinner::new().xsmall().color(p.muted_foreground))
                .child("Drawing a view…")
        })
        .when(!waiting, |d| {
            d.child(Icon::new(IconName::CircleAlert).small())
                .child("A view this version of Agentloft can't draw")
        })
        .into_any_element()
}

/// One view in the timeline: the spec it was given and where the user has
/// taken it since.
pub(crate) struct ViewCard {
    thread: WeakEntity<ThreadState>,
    store: WeakEntity<AppStore>,
    thread_id: ThreadId,
    item_id: ItemId,
    /// The tool call's input, to tell when it changes.
    source: Value,
    view: Rc<View>,
    state: Map<String, Value>,
    inputs: HashMap<u32, Entity<InputState>>,
    sliders: HashMap<u32, Entity<SliderState>>,
    /// The tab on show, per `tabs` block.
    tabs: HashMap<u32, usize>,
    /// `details` and long tables the user opened or shut.
    open: HashMap<u32, bool>,
    /// The button whose message went to the agent last.
    sent: Option<u32>,
    _subs: Vec<Subscription>,
}

impl ViewCard {
    /// `None` when the input isn't a spec this build reads.
    pub(crate) fn new(
        thread: WeakEntity<ThreadState>,
        store: WeakEntity<AppStore>,
        thread_id: ThreadId,
        item_id: ItemId,
        source: &Value,
    ) -> Option<Self> {
        let (view, _) = loft_view::parse(source).ok()?;
        Some(Self {
            thread,
            store,
            thread_id,
            item_id,
            source: source.clone(),
            state: view.state.clone(),
            view: Rc::new(view),
            inputs: HashMap::new(),
            sliders: HashMap::new(),
            tabs: HashMap::new(),
            open: HashMap::new(),
            sent: None,
            _subs: Vec::new(),
        })
    }

    /// The tool call's input as it is now. Whether the view can still be
    /// drawn (a changed spec starts the view over).
    pub(crate) fn set_source(&mut self, source: &Value, cx: &mut Context<Self>) -> bool {
        if *source == self.source {
            return true;
        }
        let Ok((view, _)) = loft_view::parse(source) else {
            return false;
        };
        self.source = source.clone();
        self.state = view.state.clone();
        self.view = Rc::new(view);
        self.inputs.clear();
        self.sliders.clear();
        self.tabs.clear();
        self.open.clear();
        self.sent = None;
        self._subs.clear();
        cx.notify();
        true
    }

    fn scope(&self) -> Scope<'_> {
        Scope::of(&self.state)
    }

    fn text(&self, template: &Template) -> String {
        template.text(&self.scope())
    }

    fn id(&self, block: u32, part: &str) -> SharedString {
        SharedString::from(format!("view-{}-{block}-{part}", self.item_id))
    }

    /// The view changed shape: its row is measured again.
    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let id = self.item_id.clone();
        if let Some(thread) = self.thread.upgrade() {
            // After this update: a handler may be running inside the thread's.
            cx.defer(move |cx| thread.update(cx, |t, cx| t.remeasure(&id, cx)));
        }
    }

    fn set(&mut self, name: &str, value: Value, cx: &mut Context<Self>) {
        if self.state.get(name) == Some(&value) {
            return;
        }
        self.state.insert(name.to_owned(), value);
        self.changed(cx);
    }

    fn run(&mut self, block: u32, action: &Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::Send(template) => {
                let text = self.text(template).trim().to_owned();
                if text.is_empty() {
                    return;
                }
                let thread_id = self.thread_id.clone();
                if let Some(store) = self.store.upgrade() {
                    store.update(cx, |s, cx| s.say(thread_id, text, cx));
                }
                self.sent = Some(block);
                cx.notify();
            }
            Action::Set(changes) => {
                // All worked out from the state as it was, then applied.
                let values: Vec<(String, Value)> = changes
                    .iter()
                    .map(|(name, template)| (name.clone(), template.eval(&self.scope())))
                    .collect();
                for (name, value) in values {
                    self.state.insert(name, value);
                }
                self.sync_inputs(window, cx);
                self.changed(cx);
            }
            Action::Copy(template) => {
                let text = self.text(template);
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                crate::items::mark_copied(self.id(block, "copy").to_string(), window, cx);
                cx.notify();
            }
            Action::Open(template) => {
                let target = self.text(template).trim().to_owned();
                if target.starts_with("http://") || target.starts_with("https://") {
                    cx.open_url(&target);
                } else if !target.is_empty() && !target.contains("://") {
                    let thread_id = self.thread_id.clone();
                    let Some(store) = self.store.upgrade() else {
                        return;
                    };
                    let cwd = self
                        .thread
                        .upgrade()
                        .and_then(|t| t.read(cx).model.as_ref().map(|m| m.thread.cwd.clone()))
                        .unwrap_or_default();
                    let path = target
                        .strip_prefix("file://")
                        .unwrap_or(&target)
                        .split_once(":")
                        .map_or(target.as_str(), |(path, _line)| path);
                    let rel = crate::items::relative_to(path, &cwd);
                    store.update(cx, |s, cx| s.open_file(thread_id, rel, cx));
                }
            }
        }
    }

    /// Sliders and text fields follow state a button changed under them.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let mut stack: Vec<&Block> = view.body.iter().collect();
        while let Some(block) = stack.pop() {
            match &block.kind {
                Kind::Slider { bind, min, max, .. } => {
                    if let (Some(slider), Some(value)) = (
                        self.sliders.get(&block.id),
                        self.state.get(bind).and_then(expr::number),
                    ) {
                        let value = value.clamp(*min, *max) as f32;
                        slider.update(cx, |s, cx| {
                            if s.value().start() != value {
                                s.set_value(value, window, cx);
                            }
                        });
                    }
                }
                Kind::Input { bind, .. } => {
                    if let Some(input) = self.inputs.get(&block.id) {
                        let value = self.state.get(bind).map(expr::text).unwrap_or_default();
                        input.update(cx, |i, cx| {
                            if i.value() != value {
                                i.set_value(value, window, cx);
                            }
                        });
                    }
                }
                Kind::Row { children, .. }
                | Kind::Column { children, .. }
                | Kind::Grid { children, .. }
                | Kind::Card { children, .. }
                | Kind::Details { children, .. } => stack.extend(children),
                Kind::Tabs { tabs } => stack.extend(tabs.iter().flat_map(|t| &t.children)),
                _ => {}
            }
        }
    }

    // -- blocks --------------------------------------------------------------

    fn blocks(
        &mut self,
        blocks: &[Block],
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(bool, AnyElement)> {
        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            if block
                .when
                .as_ref()
                .is_none_or(|when| when.truthy(&self.scope()))
            {
                out.push((block.kind.fills(), self.block(block, p, window, cx)));
            }
        }
        out
    }

    fn stack(
        &mut self,
        blocks: &[Block],
        gap: Gap,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        v_flex()
            .w_full()
            .min_w_0()
            .gap(gap_px(gap))
            .children(natural(self.blocks(blocks, p, window, cx)))
    }

    fn block(
        &mut self,
        block: &Block,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = block.id;
        match &block.kind {
            Kind::Row {
                children,
                gap,
                align,
                justify,
                wrap: _,
            } => {
                let cells = self.blocks(children, p, window, cx);
                // Tiles line up edge to edge; a mix of text and controls
                // sits on one centre line.
                let all_fill = cells.iter().all(|(fills, _)| *fills);
                let align = align.unwrap_or(if all_fill {
                    Align::Stretch
                } else {
                    Align::Center
                });
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .w_full()
                    .min_w_0()
                    .gap(gap_px(*gap))
                    .map(|d| match align {
                        Align::Start => d.items_start(),
                        Align::Center => d.items_center(),
                        Align::End => d.items_end(),
                        Align::Stretch => d,
                    })
                    .map(|d| match justify {
                        Justify::Start => d,
                        Justify::Center => d.justify_center(),
                        Justify::End => d.justify_end(),
                        Justify::Between => d.justify_between(),
                    })
                    .children(cells.into_iter().map(|(fills, cell)| {
                        if fills {
                            div()
                                .flex_1()
                                .min_w(px(132.))
                                .flex()
                                .flex_col()
                                .child(cell)
                                .into_any_element()
                        } else {
                            cell
                        }
                    }))
                    .into_any_element()
            }
            Kind::Column { children, gap } => {
                self.stack(children, *gap, p, window, cx).into_any_element()
            }
            Kind::Grid { columns, children } => {
                let cells: Vec<AnyElement> = self
                    .blocks(children, p, window, cx)
                    .into_iter()
                    .map(|(_, e)| e)
                    .collect();
                let mut rows = v_flex().w_full().gap(gap_px(Gap::Md));
                let mut cells = cells.into_iter().peekable();
                while cells.peek().is_some() {
                    let mut row = div().flex().flex_row().w_full().gap(gap_px(Gap::Md));
                    for _ in 0..*columns {
                        // An empty cell keeps the last row's widths.
                        row = row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .children(cells.next()),
                        );
                    }
                    rows = rows.child(row);
                }
                rows.into_any_element()
            }
            Kind::Card { title, children } => {
                let title = title.as_ref().map(|t| self.text(t));
                v_flex()
                    .size_full()
                    .min_w_0()
                    .gap_2()
                    .p_3()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border)
                    .when_some(title.filter(|t| !t.is_empty()), |d, title| {
                        d.child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.foreground)
                                .child(title),
                        )
                    })
                    .child(self.stack(children, Gap::Sm, p, window, cx).text_sm())
                    .into_any_element()
            }
            Kind::Tabs { tabs } => self.tabs(id, tabs, p, window, cx),
            Kind::Details {
                summary,
                open,
                children,
            } => {
                let is_open = self.open.get(&id).copied().unwrap_or(*open);
                let summary = self.text(summary);
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        h_flex()
                            .id(self.id(id, "details"))
                            .gap_1p5()
                            .text_sm()
                            .cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                            .child(
                                Icon::new(if is_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall(),
                            )
                            .child(div().min_w_0().child(summary))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open.insert(id, !is_open);
                                this.changed(cx);
                            })),
                    )
                    .when(is_open, |d| {
                        d.child(
                            div()
                                .ml(px(5.))
                                .pl_3()
                                .border_l_1()
                                .border_color(p.border)
                                .child(self.stack(children, Gap::Sm, p, window, cx).text_sm()),
                        )
                    })
                    .into_any_element()
            }
            Kind::Divider => div().h(px(1.)).w_full().bg(p.border).into_any_element(),
            Kind::Text {
                text,
                size,
                weight,
                tone,
                mono,
            } => div()
                .min_w_0()
                .map(|d| match size {
                    Size::Xs => d.text_xs(),
                    Size::Sm => d.text_sm(),
                    Size::Md => d,
                    Size::Lg => d.text_lg(),
                    Size::Xl => d.text_size(px(24.)).line_height(px(30.)),
                })
                .map(|d| match weight {
                    Weight::Normal => d,
                    Weight::Medium => d.font_weight(FontWeight::MEDIUM),
                    Weight::Bold => d.font_weight(FontWeight::SEMIBOLD),
                })
                .text_color(match tone {
                    Tone::Default => p.foreground,
                    Tone::Muted => p.muted_foreground,
                    other => tone_color(*other, p),
                })
                .when(*mono, |d| d.font_family(p.mono.clone()))
                .child(self.text(text))
                .into_any_element(),
            Kind::Heading { text, level } => div()
                .min_w_0()
                .map(|d| match level {
                    1 => d.text_lg().font_weight(FontWeight::SEMIBOLD),
                    2 => d.font_weight(FontWeight::SEMIBOLD),
                    _ => d
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.muted_foreground),
                })
                .child(self.text(text))
                .into_any_element(),
            Kind::Markdown { text } => self.markdown(id, "md", self.text(text)),
            Kind::Badge { text, tone } => badge(self.text(text), *tone, p),
            Kind::Stat {
                label,
                value,
                delta,
                delta_tone,
                caption,
                tone,
            } => {
                let delta = delta
                    .as_ref()
                    .map(|d| self.text(d))
                    .filter(|d| !d.trim().is_empty());
                let caption = caption
                    .as_ref()
                    .map(|c| self.text(c))
                    .filter(|c| !c.is_empty());
                v_flex()
                    .size_full()
                    .min_w_0()
                    .gap_1()
                    .px_3()
                    .py_2p5()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .when(!matches!(tone, Tone::Default | Tone::Muted), |d| {
                                d.child(dot(tone_color(*tone, p)))
                            })
                            .child(div().min_w_0().truncate().child(self.text(label))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_size(px(22.))
                                    .line_height(px(28.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.foreground)
                                    .child(self.text(value)),
                            )
                            .when_some(delta, |d, delta| {
                                let tone = delta_tone.unwrap_or_else(|| delta_direction(&delta));
                                d.child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(match tone {
                                            Tone::Default | Tone::Muted => p.muted_foreground,
                                            other => tone_color(other, p),
                                        })
                                        .child(delta),
                                )
                            }),
                    )
                    .when_some(caption, |d, caption| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child(caption),
                        )
                    })
                    .into_any_element()
            }
            Kind::Progress {
                value,
                max,
                label,
                tone,
            } => {
                let scope = self.scope();
                let max = max.number(&scope).filter(|m| *m > 0.0).unwrap_or(100.0);
                let fraction = (value.number(&scope).unwrap_or(0.0) / max).clamp(0.0, 1.0);
                let label = label.as_ref().map(|l| self.text(l));
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(p.muted_foreground)
                                    .children(label),
                            )
                            .child(
                                div().font_weight(FontWeight::MEDIUM).child(
                                    Format::Percent.apply((fraction * 100.0).round() / 100.0),
                                ),
                            ),
                    )
                    .child(
                        div().h(px(6.)).w_full().rounded_full().bg(p.muted).child(
                            div()
                                .h_full()
                                .rounded_full()
                                .w(relative(fraction as f32))
                                .bg(match tone {
                                    Tone::Default | Tone::Muted => p.accent,
                                    other => tone_color(*other, p),
                                }),
                        ),
                    )
                    .into_any_element()
            }
            Kind::List { items } => self.list(id, items, p, cx),
            Kind::KeyValue { items } => {
                let last = items.len().saturating_sub(1);
                v_flex()
                    .w_full()
                    .children(items.iter().enumerate().map(|(ix, (label, value))| {
                        h_flex()
                            .items_start()
                            .gap_3()
                            .py_1p5()
                            .text_sm()
                            .when(ix < last, |d| {
                                d.border_b_1().border_color(p.border.opacity(0.6))
                            })
                            .child(
                                div()
                                    .w(relative(0.32))
                                    .flex_none()
                                    .text_color(p.muted_foreground)
                                    .child(self.text(label)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(p.foreground)
                                    .child(self.text(value)),
                            )
                    }))
                    .into_any_element()
            }
            Kind::Steps { items } => self.steps(items, p),
            Kind::Callout { title, text, tone } => {
                let color = match tone {
                    Tone::Default | Tone::Muted => p.muted_foreground,
                    other => tone_color(*other, p),
                };
                let icon = match tone {
                    Tone::Success => IconName::CircleCheck,
                    Tone::Warning => IconName::TriangleAlert,
                    Tone::Danger => IconName::CircleAlert,
                    _ => IconName::Info,
                };
                let title = title
                    .as_ref()
                    .map(|t| self.text(t))
                    .filter(|t| !t.is_empty());
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2p5()
                    .px_3()
                    .py_2p5()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(color.opacity(0.35))
                    .bg(color.opacity(0.08))
                    .child(
                        div()
                            .pt(px(2.))
                            .text_color(color)
                            .child(Icon::new(icon).small()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .text_sm()
                            .when_some(title, |d, title| {
                                d.child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(p.foreground)
                                        .child(title),
                                )
                            })
                            .child(self.markdown(id, "callout", self.text(text))),
                    )
                    .into_any_element()
            }
            Kind::Code { text, language } => {
                let code = self.text(text);
                // A fence longer than any run of backticks in the code.
                let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat(longest.max(2) + 1);
                let language = language.as_deref().unwrap_or_default();
                self.markdown(id, "code", format!("{fence}{language}\n{code}\n{fence}"))
            }
            Kind::Table { columns, rows } => self.table(id, columns, rows, p, cx),
            Kind::Chart(chart) => self.chart(id, chart, p),
            Kind::Slider {
                bind,
                min,
                max,
                step,
                label,
                format,
            } => {
                let slider = match self.sliders.get(&id) {
                    Some(slider) => slider.clone(),
                    None => {
                        let start = self
                            .state
                            .get(bind)
                            .and_then(expr::number)
                            .unwrap_or(*min)
                            .clamp(*min, *max);
                        let (min, max, step) = (*min as f32, *max as f32, *step as f32);
                        let slider = cx.new(|_| {
                            SliderState::new()
                                .min(min)
                                .max(max)
                                .step(step)
                                .default_value(start as f32)
                        });
                        let name = bind.clone();
                        let whole = step.fract() == 0.0;
                        self._subs.push(cx.subscribe(
                            &slider,
                            move |this, _, event: &SliderEvent, cx| {
                                let (SliderEvent::Change(value) | SliderEvent::Release(value)) =
                                    event;
                                // f32 steps drift (0.30000001): round to
                                // what the step can mean.
                                let value = f64::from(value.start());
                                let value = if whole {
                                    value.round()
                                } else {
                                    (value * 1e4).round() / 1e4
                                };
                                this.set(&name, expr::num_value(value), cx);
                            },
                        ));
                        self.sliders.insert(id, slider.clone());
                        slider
                    }
                };
                let value = self.state.get(bind).and_then(expr::number).unwrap_or(*min);
                let shown = match format {
                    Some(format) => format.apply(value),
                    None => expr::text(&expr::num_value(value)),
                };
                let label = label.as_ref().map(|l| self.text(l));
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(p.muted_foreground)
                                    .children(label),
                            )
                            .child(div().font_weight(FontWeight::MEDIUM).child(shown)),
                    )
                    .child(Slider::new(&slider))
                    .into_any_element()
            }
            Kind::Input {
                bind,
                label,
                placeholder,
                number,
            } => {
                let input = match self.inputs.get(&id) {
                    Some(input) => input.clone(),
                    None => {
                        let start = self.state.get(bind).map(expr::text).unwrap_or_default();
                        let placeholder = placeholder.clone().unwrap_or_default();
                        let input = cx.new(|cx| {
                            InputState::new(window, cx)
                                .placeholder(placeholder)
                                .default_value(start)
                        });
                        let (name, number) = (bind.clone(), *number);
                        self._subs.push(cx.subscribe(
                            &input,
                            move |this, input, event: &InputEvent, cx| {
                                if !matches!(event, InputEvent::Change) {
                                    return;
                                }
                                let typed = input.read(cx).value().to_string();
                                let value = if number {
                                    expr::number(&Value::String(typed))
                                        .map_or(Value::Null, expr::num_value)
                                } else {
                                    Value::String(typed)
                                };
                                this.set(&name, value, cx);
                            },
                        ));
                        self.inputs.insert(id, input.clone());
                        input
                    }
                };
                labelled(
                    label.as_ref().map(|l| self.text(l)),
                    Input::new(&input).small().into_any_element(),
                    p,
                )
            }
            Kind::Select {
                bind,
                label,
                options,
            } => {
                let current = self.state.get(bind).cloned().unwrap_or(Value::Null);
                let shown = options
                    .iter()
                    .find(|o| o.value == current)
                    .map(|o| self.text(&o.label))
                    .unwrap_or_else(|| expr::text(&current));
                let entries: Vec<(String, Value)> = options
                    .iter()
                    .map(|o| (self.text(&o.label), o.value.clone()))
                    .collect();
                let (card, name) = (cx.entity().downgrade(), bind.clone());
                let button = Button::new(self.id(id, "select"))
                    .label(shown)
                    .dropdown_caret(true)
                    .outline()
                    .small()
                    .cursor_pointer()
                    .dropdown_menu(move |mut menu, _, _| {
                        for (label, value) in &entries {
                            let (card, name, value) = (card.clone(), name.clone(), value.clone());
                            let checked = value == current;
                            menu = menu.item(
                                PopupMenuItem::new(label.clone()).checked(checked).on_click(
                                    move |_, _, cx| {
                                        let (name, value) = (name.clone(), value.clone());
                                        let _ =
                                            card.update(cx, |this, cx| this.set(&name, value, cx));
                                    },
                                ),
                            );
                        }
                        menu
                    });
                labelled(
                    label.as_ref().map(|l| self.text(l)),
                    h_flex().child(button).into_any_element(),
                    p,
                )
            }
            Kind::Choice {
                bind,
                label,
                options,
                multiple,
            } => {
                let pills = self.choice(id, bind, options, *multiple, p, cx);
                labelled(label.as_ref().map(|l| self.text(l)), pills, p)
            }
            Kind::Checkbox { bind, label } => {
                let name = bind.clone();
                control_height(
                    Checkbox::new(self.id(id, "check"))
                        .label(self.text(label))
                        .checked(self.state.get(bind).is_some_and(expr::truthy))
                        .on_click(cx.listener(move |this, on: &bool, _, cx| {
                            this.set(&name, Value::Bool(*on), cx);
                        })),
                )
            }
            Kind::Toggle { bind, label } => {
                let name = bind.clone();
                control_height(
                    Switch::new(self.id(id, "toggle"))
                        .small()
                        .label(self.text(label))
                        .checked(self.state.get(bind).is_some_and(expr::truthy))
                        .on_click(cx.listener(move |this, on: &bool, _, cx| {
                            this.set(&name, Value::Bool(*on), cx);
                        })),
                )
            }
            Kind::Button {
                label,
                action,
                style,
            } => {
                let copy_id = self.id(id, "copy").to_string();
                let copied = crate::items::just_copied(&copy_id);
                let sent = self.sent == Some(id);
                let tip = match action {
                    Action::Send(t) => format!("Says: {}", clip(&self.text(t), 160)),
                    Action::Copy(_) if copied => "Copied".to_owned(),
                    Action::Copy(t) => format!("Copies: {}", clip(&self.text(t), 80)),
                    Action::Open(t) => format!("Opens {}", clip(&self.text(t), 120)),
                    Action::Set(_) => String::new(),
                };
                let action = action.clone();
                Button::new(self.id(id, "button"))
                    .label(self.text(label))
                    .small()
                    .cursor_pointer()
                    .map(|b| match style {
                        ButtonStyle::Primary => b.primary(),
                        ButtonStyle::Secondary => b.outline(),
                        ButtonStyle::Ghost => b.ghost(),
                        ButtonStyle::Danger => b.danger(),
                    })
                    .when(sent || copied, |b| b.icon(IconName::Check))
                    .when(!tip.is_empty(), |b| b.tooltip(tip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run(id, &action, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }

    /// Markdown inside a view: selectable, with the timeline's code blocks.
    fn markdown(&self, block: u32, part: &str, text: String) -> AnyElement {
        crate::items::code_blocks(TextView::markdown(self.id(block, part), text).selectable(true))
            .into_any_element()
    }

    fn tabs(
        &mut self,
        id: u32,
        tabs: &[Tab],
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.tabs.get(&id).copied().unwrap_or(0).min(tabs.len() - 1);
        let strip = h_flex()
            .w_full()
            .gap_4()
            .border_b_1()
            .border_color(p.border)
            .children(tabs.iter().enumerate().map(|(ix, tab)| {
                let on = ix == current;
                div()
                    .id(self.id(id, &format!("tab-{ix}")))
                    .pb_1p5()
                    // The selected tab's line sits on the strip's.
                    .mb(px(-1.))
                    .border_b_2()
                    .text_sm()
                    .cursor_pointer()
                    .when(on, |d| {
                        d.border_color(p.accent)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!on, |d| {
                        d.border_color(gpui_kit::transparent_black())
                            .text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .child(self.text(&tab.label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tabs.insert(id, ix);
                        this.changed(cx);
                    }))
            }));
        v_flex()
            .w_full()
            .gap_3()
            .child(strip)
            .child(self.stack(&tabs[current].children, Gap::Md, p, window, cx))
            .into_any_element()
    }

    fn list(
        &mut self,
        id: u32,
        items: &[ListItem],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = items.len().saturating_sub(1);
        v_flex()
            .w_full()
            .children(items.iter().enumerate().map(|(ix, item)| {
                let text = item
                    .text
                    .as_ref()
                    .map(|t| self.text(t))
                    .filter(|t| !t.is_empty());
                let badge_text = item
                    .badge
                    .as_ref()
                    .map(|b| self.text(b))
                    .filter(|b| !b.is_empty());
                let toned = !matches!(item.tone, Tone::Default | Tone::Muted);
                let row = h_flex()
                    .id(self.id(id, &format!("item-{ix}")))
                    .w_full()
                    .gap_2p5()
                    .px_1p5()
                    .py_1p5()
                    .rounded(px(6.))
                    .when(ix < last, |d| {
                        d.border_b_1().border_color(p.border.opacity(0.6))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(p.foreground)
                                    .child(self.text(&item.title)),
                            )
                            .when_some(text, |d, text| {
                                d.child(div().text_xs().text_color(p.muted_foreground).child(text))
                            }),
                    )
                    .when_some(badge_text, |d, text| d.child(badge(text, item.tone, p)))
                    .when(toned && item.badge.is_none(), |d| {
                        d.child(dot(tone_color(item.tone, p)))
                    });
                match &item.action {
                    Some(action) => {
                        let action = action.clone();
                        let sent = self.sent == Some(id * 1000 + ix as u32);
                        row.cursor_pointer()
                            .hover(|d| d.bg(p.hover))
                            .child(
                                Icon::new(if sent {
                                    IconName::Check
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .text_color(p.muted_foreground),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run(id * 1000 + ix as u32, &action, window, cx);
                            }))
                    }
                    None => row,
                }
            }))
            .into_any_element()
    }

    fn steps(&self, items: &[Step], p: &Palette) -> AnyElement {
        let last = items.len().saturating_sub(1);
        v_flex()
            .w_full()
            .children(items.iter().enumerate().map(|(ix, step)| {
                let (icon, color) = match step.status {
                    StepStatus::Done => (IconName::CircleCheck, p.success),
                    StepStatus::Active => (IconName::CircleDot, p.accent),
                    StepStatus::Pending => (IconName::Circle, p.faint_foreground),
                    StepStatus::Failed => (IconName::CircleX, p.danger),
                };
                let text = step
                    .text
                    .as_ref()
                    .map(|t| self.text(t))
                    .filter(|t| !t.is_empty());
                let pending = step.status == StepStatus::Pending;
                div()
                    .flex()
                    .flex_row()
                    .gap_2p5()
                    .child(
                        // The marker, and the line down to the next one.
                        v_flex()
                            .items_center()
                            .flex_none()
                            .pt(px(2.))
                            .child(Icon::new(icon).small().text_color(color))
                            .when(ix < last, |d| {
                                d.child(div().flex_1().w(px(1.)).mt(px(3.)).bg(p.border))
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .pb(if ix < last { px(12.) } else { px(0.) })
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if pending {
                                        p.muted_foreground
                                    } else {
                                        p.foreground
                                    })
                                    .when(step.status == StepStatus::Active, |d| {
                                        d.font_weight(FontWeight::MEDIUM)
                                    })
                                    .child(self.text(&step.title)),
                            )
                            .when_some(text, |d, text| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(if step.status == StepStatus::Failed {
                                            p.danger
                                        } else {
                                            p.muted_foreground
                                        })
                                        .child(text),
                                )
                            }),
                    )
            }))
            .into_any_element()
    }

    fn choice(
        &mut self,
        id: u32,
        bind: &str,
        options: &[Choice],
        multiple: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.state.get(bind).cloned().unwrap_or(Value::Null);
        let picked = |value: &Value| match &current {
            Value::Array(list) if multiple => list.contains(value),
            other => other == value,
        };
        // One choice of several reads as a segmented control; several of
        // several as chips that each switch on and off.
        let mut strip = h_flex().flex_wrap().gap_1();
        if !multiple {
            strip = strip.p(px(2.)).rounded(px(8.)).bg(p.muted);
        }
        strip
            .children(options.iter().enumerate().map(|(ix, option)| {
                let on = picked(&option.value);
                let (name, value) = (bind.to_owned(), option.value.clone());
                div()
                    .id(self.id(id, &format!("opt-{ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2p5()
                    .py(px(3.))
                    .text_sm()
                    .cursor_pointer()
                    .map(|d| {
                        if multiple {
                            d.rounded_full().border_1().border_color(if on {
                                p.accent
                            } else {
                                p.border
                            })
                        } else {
                            d.rounded(px(6.))
                        }
                    })
                    .when(on && multiple, |d| {
                        d.bg(p.accent_soft)
                            .text_color(p.foreground)
                            .child(Icon::new(IconName::Check).xsmall())
                    })
                    .when(on && !multiple, |d| {
                        d.bg(p.background)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .shadow_sm()
                    })
                    .when(!on, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.text_color(p.foreground))
                    })
                    .child(self.text(&option.label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if multiple {
                            let mut list = match this.state.get(&name) {
                                Some(Value::Array(list)) => list.clone(),
                                _ => Vec::new(),
                            };
                            match list.iter().position(|v| *v == value) {
                                Some(at) => {
                                    list.remove(at);
                                }
                                None => list.push(value.clone()),
                            }
                            Value::Array(list)
                        } else {
                            value.clone()
                        };
                        this.set(&name, next, cx);
                    }))
            }))
            .into_any_element()
    }

    fn table(
        &mut self,
        id: u32,
        columns: &[Column],
        data: &Data,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = data.rows(&self.scope());
        // A column of numbers lines up on the right.
        let numeric: Vec<bool> = columns
            .iter()
            .map(|c| {
                let mut cells = rows.iter().filter_map(|r| r.get(&c.key)).filter(|v| {
                    !matches!(v, Value::Null) && !matches!(v, Value::String(s) if s.is_empty())
                });
                let mut any = false;
                cells.all(|v| {
                    any = true;
                    v.is_number()
                }) && any
            })
            .collect();
        // A percent column keeps one precision top to bottom (42.3%, 13.0%).
        let tenths: Vec<bool> = columns
            .iter()
            .map(|c| {
                c.format == Some(Format::Percent)
                    && rows
                        .iter()
                        .filter_map(|r| r.get(&c.key).and_then(expr::number))
                        .any(|n| ((n * 100.0).round() - n * 100.0).abs() > 1e-9)
            })
            .collect();
        let cell_text = |ix: usize, value: Option<&Value>| match (value, columns[ix].format) {
            (None | Some(Value::Null), _) => String::new(),
            (Some(v), Some(format)) => match expr::number(v) {
                Some(n) if tenths[ix] => format!("{:.1}%", n * 100.0),
                Some(n) => format.apply(n),
                None => expr::text(v),
            },
            (Some(v), None) => expr::text(v),
        };
        // Each column gets room for what it holds, within reason.
        let labels: Vec<String> = columns.iter().map(|c| self.text(&c.label)).collect();
        let weights: Vec<f32> = columns
            .iter()
            .zip(&labels)
            .enumerate()
            .map(|(ix, (column, label))| {
                let widest = rows
                    .iter()
                    .take(50)
                    .map(|r| cell_text(ix, r.get(&column.key)).chars().count())
                    .chain([label.chars().count()])
                    .max()
                    .unwrap_or(0);
                widest.clamp(6, 36) as f32
            })
            .collect();
        let total: f32 = weights.iter().sum();
        let cell = |ix: usize| {
            let align = columns[ix].align.unwrap_or(if numeric[ix] {
                ColumnAlign::Right
            } else {
                ColumnAlign::Left
            });
            div()
                .w(relative(weights[ix] / total))
                .min_w_0()
                .px_2()
                .flex()
                .map(|d| match align {
                    ColumnAlign::Left => d,
                    ColumnAlign::Center => d.justify_center(),
                    ColumnAlign::Right => d.justify_end(),
                })
        };
        let all = self.open.get(&id).copied().unwrap_or(false);
        let shown = if all || rows.len() <= TABLE_PREVIEW + 2 {
            rows.len()
        } else {
            TABLE_PREVIEW
        };
        let header = h_flex()
            .w_full()
            .pb_1p5()
            .border_b_1()
            .border_color(p.border)
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(p.muted_foreground)
            .children(labels.iter().enumerate().map(|(ix, label)| {
                cell(ix).child(div().min_w_0().truncate().child(label.clone()))
            }));
        let body = rows.iter().take(shown).enumerate().map(|(row_ix, row)| {
            h_flex()
                .w_full()
                .items_start()
                .py_1p5()
                .text_sm()
                .when(row_ix + 1 < shown, |d| {
                    d.border_b_1().border_color(p.border.opacity(0.6))
                })
                .children(columns.iter().enumerate().map(|(ix, column)| {
                    let text = cell_text(ix, row.get(&column.key));
                    cell(ix).child(
                        div()
                            .min_w_0()
                            .when(numeric[ix], |d| d.whitespace_nowrap())
                            .text_color(if ix == 0 {
                                p.foreground
                            } else {
                                p.muted_foreground
                            })
                            .child(text),
                    )
                }))
        });
        let more = (rows.len() > TABLE_PREVIEW + 2).then(|| {
            div()
                .id(self.id(id, "more"))
                .px_2()
                .pt_1p5()
                .text_xs()
                .cursor_pointer()
                .text_color(p.muted_foreground)
                .hover(|d| d.text_color(p.foreground))
                .child(if all {
                    "Show fewer".to_owned()
                } else {
                    format!("Show all {} rows", rows.len())
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open.insert(id, !all);
                    this.changed(cx);
                }))
        });
        v_flex()
            .w_full()
            .child(header)
            .children(body)
            .children(more)
            .when(rows.is_empty(), |d| {
                d.child(
                    div()
                        .px_2()
                        .py_2()
                        .text_sm()
                        .text_color(p.faint_foreground)
                        .child("No rows"),
                )
            })
            .into_any_element()
    }

    fn chart(&self, id: u32, chart: &Chart, p: &Palette) -> AnyElement {
        let rows = chart.data.rows(&self.scope());
        let colors = series_colors(p.dark);
        let mut data: Vec<Datum> = rows
            .iter()
            .map(|row| Datum {
                x: SharedString::from(row.get(&chart.x).map(expr::text).unwrap_or_default()),
                ys: chart
                    .series
                    .iter()
                    .map(|s| row.get(&s.key).and_then(expr::number).unwrap_or(0.0))
                    .collect(),
                color: colors[0],
            })
            .collect();
        if data.is_empty() {
            return div()
                .h(px(chart.height))
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(p.faint_foreground)
                .child("No data")
                .into_any_element();
        }
        // Axes read short (12k); the tooltip gives the figure in full.
        let axis = chart.format.unwrap_or(Format::Compact);
        let full = chart.format.unwrap_or(Format::Number);
        let values = || data.iter().flat_map(|d| d.ys.iter().copied());
        let (bottom, top, ticks) = nice_axis(
            values().fold(0.0, f64::min),
            values().fold(0.0, f64::max),
            axis,
        );
        let chart_id = self.id(id, "chart");
        // About eight labels along the bottom, however many points.
        let tick_margin = data.len().div_ceil(8).max(1);
        let height = px(chart.height);
        match chart.kind {
            ChartKind::Bar => {
                // A bar chart's axis ends at its tallest bar, so its ticks
                // are never round: a few bars carry their own figures
                // instead, and many fall back to the axis.
                let labelled = data.len() <= 10;
                let muted = p.muted_foreground;
                div()
                    .h(height)
                    .w_full()
                    .child(
                        BarChart::new(data)
                            .id(chart_id)
                            .band(|d: &Datum| d.x.clone())
                            .value(|d: &Datum| d.ys[0])
                            .fill(move |d: &Datum, _, _, _| d.color)
                            .corner_radii(gpui_kit::Corners {
                                top_left: px(4.),
                                top_right: px(4.),
                                bottom_left: px(0.),
                                bottom_right: px(0.),
                            })
                            .tick_margin(tick_margin)
                            .map(|chart| {
                                if labelled {
                                    chart
                                        .label(move |d: &Datum| axis.apply(d.ys[0]))
                                        .label_color(move |_: &Datum| muted)
                                } else {
                                    chart
                                        .value_axis(true)
                                        .value_tick_format(move |v: f64| axis.apply(v))
                                }
                            })
                            .tooltip_value(move |_: &Datum, v: f64| full.apply(v).into()),
                    )
                    .into_any_element()
            }
            ChartKind::Line if chart.series.len() == 1 => div()
                .h(height)
                .w_full()
                .child(
                    LineChart::new(data)
                        .id(chart_id)
                        .x(|d: &Datum| d.x.clone())
                        .y(|d: &Datum| d.ys[0])
                        .stroke(colors[0])
                        .dot()
                        .tick_margin(tick_margin)
                        .y_axis(true)
                        .y_domain(bottom, top)
                        .y_padding(0., 0.)
                        .y_tick_count(ticks)
                        .y_tick_format(move |v: f64| axis.apply(v))
                        .tooltip_value(move |_: &Datum, v: f64| full.apply(v).into()),
                )
                .into_any_element(),
            ChartKind::Line | ChartKind::Area => {
                // Lines keep their names by colour and by legend; an area's
                // fill is a wash of the same hue.
                let wash = if chart.kind == ChartKind::Area {
                    0.16
                } else {
                    0.0
                };
                let mut plot = AreaChart::new(data)
                    .id(chart_id)
                    .x(|d: &Datum| d.x.clone())
                    .tick_margin(tick_margin)
                    .y_axis(true)
                    .y_domain(bottom, top)
                    .y_padding(0., 0.)
                    .y_tick_count(ticks)
                    .y_tick_format(move |v: f64| axis.apply(v))
                    .tooltip_value(move |_: &Datum, _, v: f64| full.apply(v).into());
                for (ix, series) in chart.series.iter().enumerate() {
                    let color = colors[ix % colors.len()];
                    plot = plot
                        .y(move |d: &Datum| d.ys[ix])
                        .stroke(color)
                        .fill(color.opacity(wash))
                        .name(series.label.clone());
                }
                v_flex()
                    .w_full()
                    .gap_2()
                    .when(chart.series.len() > 1, |d| {
                        d.child(h_flex().flex_wrap().gap_x_4().gap_y_1().children(
                            chart.series.iter().enumerate().map(|(ix, series)| {
                                legend(series.label.clone(), colors[ix % colors.len()], p)
                            }),
                        ))
                    })
                    .child(div().h(height).w_full().child(plot))
                    .into_any_element()
            }
            ChartKind::Pie | ChartKind::Donut => {
                // Largest first; past six, the rest are one slice.
                data.retain(|d| d.ys[0] > 0.0);
                data.sort_by(|a, b| b.ys[0].total_cmp(&a.ys[0]));
                if data.len() > PIE_SLICES {
                    let rest: f64 = data[PIE_SLICES - 1..].iter().map(|d| d.ys[0]).sum();
                    data.truncate(PIE_SLICES - 1);
                    data.push(Datum {
                        x: "Other".into(),
                        ys: vec![rest],
                        color: p.muted_foreground.opacity(0.5),
                    });
                }
                for (ix, datum) in data.iter_mut().enumerate() {
                    if datum.x != "Other" || ix < PIE_SLICES - 1 {
                        datum.color = colors[ix % colors.len()];
                    }
                }
                let total: f64 = data.iter().map(|d| d.ys[0]).sum();
                let side = chart.height.min(220.);
                let outer = side * 0.46;
                let key = v_flex()
                    .flex_1()
                    .min_w(px(150.))
                    .gap_1()
                    .children(data.iter().map(|d| {
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(dot(d.color))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(p.foreground)
                                    .child(d.x.clone()),
                            )
                            .child(
                                div()
                                    .text_color(p.muted_foreground)
                                    .child(full.apply(d.ys[0])),
                            )
                            .child(
                                div()
                                    .w(px(40.))
                                    .flex()
                                    .justify_end()
                                    .text_color(p.faint_foreground)
                                    .child(Format::Percent.apply(
                                        ((d.ys[0] / total.max(f64::MIN_POSITIVE)) * 100.0).round()
                                            / 100.0,
                                    )),
                            )
                    }));
                let plot = PieChart::new(data)
                    .id(chart_id)
                    .value(|d: &Datum| d.ys[0] as f32)
                    .color(|d: &Datum| d.color)
                    .outer_radius(outer)
                    .inner_radius(if chart.kind == ChartKind::Donut {
                        outer * 0.62
                    } else {
                        0.
                    })
                    .pad_angle(0.02)
                    .tooltip_name(|d: &Datum| d.x.clone())
                    .tooltip_value(move |_: &Datum, v: f32, share: f32| {
                        format!("{} ({share:.0}%)", full.apply(f64::from(v))).into()
                    });
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .justify_center()
                    .gap_4()
                    .child(div().size(px(side)).flex_none().child(plot))
                    .child(key)
                    .into_any_element()
            }
        }
    }
}

impl Render for ViewCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let view = self.view.clone();
        let title = view
            .title
            .as_ref()
            .map(|t| self.text(t))
            .filter(|t| !t.is_empty());
        v_flex()
            .w_full()
            .gap_3()
            .px_4()
            .py_3p5()
            .rounded(px(12.))
            .border_1()
            .border_color(p.border)
            .bg(p.elevated)
            .text_color(p.foreground)
            .when_some(title, |d, title| {
                d.child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.foreground)
                        .child(title),
                )
            })
            .children(natural(self.blocks(&view.body, &p, window, cx)))
    }
}

/// Blocks down a column: tiles and charts take its width, while a button
/// or a badge keeps its own (a column would otherwise stretch it).
fn natural(cells: Vec<(bool, AnyElement)>) -> impl Iterator<Item = AnyElement> {
    cells.into_iter().map(|(fills, cell)| {
        if fills {
            cell
        } else {
            div()
                .flex()
                .flex_row()
                .min_w_0()
                .child(cell)
                .into_any_element()
        }
    })
}

/// One row of a chart.
#[derive(Clone)]
struct Datum {
    x: SharedString,
    ys: Vec<f64>,
    color: Hsla,
}

/// The hues of chart series, in the order they are handed out (never
/// cycled within a chart: the spec caps series at six). The steps differ
/// per mode so neighbours stay apart for colour-blind readers too.
fn series_colors(dark: bool) -> [Hsla; 6] {
    let hex: [u32; 6] = if dark {
        [0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300]
    } else {
        [0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300]
    };
    hex.map(|c| rgb(c).into())
}

fn tone_color(tone: Tone, p: &Palette) -> Hsla {
    match tone {
        Tone::Default => p.foreground,
        Tone::Muted => p.muted_foreground,
        Tone::Accent => p.accent,
        Tone::Info => p.info,
        Tone::Success => p.success,
        Tone::Warning => p.warning,
        Tone::Danger => p.danger,
    }
}

/// A delta's tone from its sign: up is good unless the agent says otherwise.
fn delta_direction(delta: &str) -> Tone {
    match delta.trim().chars().next() {
        Some('+' | '↑' | '▲') => Tone::Success,
        Some('-' | '−' | '–' | '↓' | '▼') => Tone::Danger,
        _ => Tone::Muted,
    }
}

fn gap_px(gap: Gap) -> Pixels {
    match gap {
        Gap::None => px(0.),
        Gap::Sm => px(6.),
        Gap::Md => px(12.),
        Gap::Lg => px(20.),
    }
}

fn dot(color: Hsla) -> gpui_kit::Div {
    div().flex_none().size(px(7.)).rounded_full().bg(color)
}

/// A pill: its tone is a dot beside the words, never the words' own colour.
fn badge(text: String, tone: Tone, p: &Palette) -> AnyElement {
    let toned = !matches!(tone, Tone::Default | Tone::Muted);
    h_flex()
        .flex_none()
        .gap_1p5()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .map(|d| {
            if toned {
                d.bg(tone_color(tone, p).opacity(0.12))
                    .text_color(p.foreground)
                    .child(dot(tone_color(tone, p)))
            } else {
                d.bg(p.muted).text_color(p.muted_foreground)
            }
        })
        .child(text)
        .into_any_element()
}

fn legend(label: String, color: Hsla, p: &Palette) -> gpui_kit::Div {
    h_flex()
        .gap_1p5()
        .text_xs()
        .text_color(p.muted_foreground)
        .child(
            div()
                .flex_none()
                .w(px(10.))
                .h(px(3.))
                .rounded_full()
                .bg(color),
        )
        .child(label)
}

/// A switch or checkbox in a box as tall as a button, so it sits level
/// with the controls beside it.
fn control_height(control: impl IntoElement) -> AnyElement {
    h_flex().min_h(px(28.)).child(control).into_any_element()
}

/// A value axis that reads in round numbers: where it starts and ends, and
/// how many ticks it carries. Round in the unit the format shows (2.5 MB,
/// not 2,000,000 bytes).
fn nice_axis(lo: f64, hi: f64, format: Format) -> (f64, f64, usize) {
    let lo = lo.min(0.0);
    let hi = if hi > lo { hi } else { lo + 1.0 };
    let reach = hi.abs().max(lo.abs());
    let unit = match format {
        Format::Bytes => 1024f64.powi((reach.max(1.0).log(1024.0).floor() as i32).clamp(0, 4)),
        Format::Duration if reach >= 3_600_000.0 => 3_600_000.0,
        Format::Duration if reach >= 60_000.0 => 60_000.0,
        Format::Duration if reach >= 1_000.0 => 1_000.0,
        _ => 1.0,
    };
    let rough = (hi - lo) / unit / 5.0;
    let magnitude = 10f64.powf(rough.log10().floor());
    let step = match rough / magnitude {
        n if n <= 1.0 => 1.0,
        n if n <= 2.0 => 2.0,
        n if n <= 2.5 => 2.5,
        n if n <= 5.0 => 5.0,
        _ => 10.0,
    } * magnitude
        * unit;
    let top = (hi / step).ceil() * step;
    let bottom = (lo / step).floor() * step;
    let ticks = ((top - bottom) / step).round() as usize + 1;
    (bottom, top, ticks.clamp(2, 12))
}

/// A control under its label.
fn labelled(label: Option<String>, control: AnyElement, p: &Palette) -> AnyElement {
    match label.filter(|l| !l.is_empty()) {
        Some(label) => v_flex()
            .min_w_0()
            .gap_1p5()
            .child(div().text_sm().text_color(p.muted_foreground).child(label))
            .child(control)
            .into_any_element(),
        None => control,
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::ToolKind;

    fn call(tool: &str, status: ItemStatus, is_error: bool) -> Item {
        Item {
            id: ItemId("i".into()),
            thread_id: ThreadId("t".into()),
            turn_id: None,
            ordinal: 0,
            status,
            body: ItemBody::ToolCall {
                tool: tool.into(),
                tool_kind: ToolKind::Mcp,
                title: tool.into(),
                input: serde_json::json!({}),
                output: None,
                is_error,
            },
            updated_at_ms: 0,
            parent_id: None,
            started_at_ms: None,
        }
    }

    #[test]
    fn only_a_show_ui_call_that_went_through_is_a_view() {
        let tool = "mcp__agentloft__show_ui";
        assert!(is_view(&call(tool, ItemStatus::Completed, false)));
        assert!(is_view(&call(tool, ItemStatus::InProgress, false)));
        // Turned down: an ordinary failed tool line.
        assert!(!is_view(&call(tool, ItemStatus::Failed, true)));
        assert!(!is_view(&call(tool, ItemStatus::Completed, true)));
        assert!(!is_view(&call(tool, ItemStatus::Declined, false)));
        // Someone else's tool of the same name isn't ours.
        assert!(!is_view(&call(
            "mcp__other__show_ui",
            ItemStatus::Completed,
            false
        )));
        assert!(!is_view(&call(
            "mcp__agentloft__browser_click",
            ItemStatus::Completed,
            false
        )));
    }

    #[test]
    fn axes_read_in_round_numbers() {
        assert_eq!(nice_axis(0.0, 2000.0, Format::Usd), (0.0, 2000.0, 5));
        assert_eq!(nice_axis(0.0, 4500.0, Format::Usd), (0.0, 5000.0, 6));
        assert_eq!(nice_axis(0.0, 412.0, Format::Compact), (0.0, 500.0, 6));
        let (bottom, top, ticks) = nice_axis(0.0, 0.93, Format::Percent);
        assert_eq!((bottom, ticks), (0.0, 6));
        assert!((top - 1.0).abs() < 1e-9, "{top}");
        // In the unit the reader sees: megabytes, seconds.
        let mb = 1024.0 * 1024.0;
        assert_eq!(nice_axis(0.0, 9.4 * mb, Format::Bytes), (0.0, 10.0 * mb, 6));
        assert_eq!(
            nice_axis(0.0, 18_400.0, Format::Duration),
            (0.0, 20_000.0, 5)
        );
        // Below zero the axis reaches down as far as it must.
        assert_eq!(nice_axis(-30.0, 90.0, Format::Number), (-50.0, 100.0, 7));
        // Nothing to show still makes an axis.
        let (bottom, top, _) = nice_axis(0.0, 0.0, Format::Number);
        assert_eq!((bottom, top), (0.0, 1.0));
    }

    #[test]
    fn deltas_read_their_sign() {
        assert_eq!(delta_direction("+9"), Tone::Success);
        assert_eq!(delta_direction(" -12%"), Tone::Danger);
        assert_eq!(delta_direction("−3"), Tone::Danger);
        assert_eq!(delta_direction("steady"), Tone::Muted);
        assert_eq!(clip("a long sentence here", 6), "a long…");
        assert_eq!(clip("short", 6), "short");
    }
}
