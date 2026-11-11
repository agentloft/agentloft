//! Changes view: the diff of the last turn, the whole thread, or uncommitted
//! work, from git checkpoints (ADR-0008), rendered with gpui-kit's Diff.

use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{DiffResult, DiffScope, Request, Response, ThreadId};

pub struct ChangesView {
    store: Entity<AppStore>,
    thread_id: ThreadId,
    scope: DiffScope,
    loading: bool,
    result: Option<DiffResult>,
    error: Option<String>,
    diff: Option<Entity<DiffState>>,
    split: bool,
    /// A file to scroll to once the diff has loaded.
    reveal: Option<String>,
}

impl ChangesView {
    pub fn new(store: Entity<AppStore>, thread_id: ThreadId, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store,
            thread_id,
            scope: DiffScope::Thread,
            loading: false,
            result: None,
            error: None,
            diff: None,
            split: false,
            reveal: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        let body = Request::GetDiff {
            thread_id: self.thread_id.clone(),
            scope: self.scope.clone(),
        };
        request_in(&self.store, body, cx, |this, result, cx| {
            this.loading = false;
            match result {
                Ok(Response::Diff { diff }) => {
                    this.error = None;
                    let files = DiffFile::parse(&diff.patch).unwrap_or_default();
                    let mode = if this.split {
                        DiffMode::Split
                    } else {
                        DiffMode::Unified
                    };
                    let diff_state = cx.new(|cx| DiffState::new(files, cx).with_mode(mode));
                    if let Some(path) = this.reveal.take() {
                        diff_state.update(cx, |d, cx| d.scroll_to_file(&path, cx));
                    }
                    this.diff = Some(diff_state);
                    this.result = Some(diff);
                }
                Ok(other) => this.error = Some(format!("unexpected response: {other:?}")),
                Err(err) => this.error = Some(err.message),
            }
            cx.notify();
        });
    }

    /// Show what one turn changed, scrolled to `path` if set.
    pub fn show_turn(
        &mut self,
        turn_id: loft_protocol::TurnId,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let scope = DiffScope::Turn(turn_id);
        if self.scope == scope {
            if self.loading {
                self.reveal = path;
            } else if let (Some(path), Some(diff)) = (&path, &self.diff) {
                diff.update(cx, |d, cx| d.scroll_to_file(path, cx));
            }
            return;
        }
        self.reveal = path;
        self.scope = scope;
        self.refresh(cx);
    }

    /// "Turn 3" for the turn the view is scoped to.
    fn turn_label(&self, cx: &Context<Self>) -> Option<String> {
        let DiffScope::Turn(id) = &self.scope else {
            return None;
        };
        let store = self.store.read(cx);
        let ix = store
            .threads
            .get(&self.thread_id)
            .and_then(|t| t.read(cx).model.as_ref())
            .and_then(|m| m.turns.iter().position(|t| &t.id == id));
        Some(match ix {
            Some(ix) => format!("Turn {}", ix + 1),
            None => "Turn".to_owned(),
        })
    }

    fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.refresh(cx);
        }
    }

    fn toggle_split(&mut self, cx: &mut Context<Self>) {
        self.split = !self.split;
        let mode = if self.split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        if let Some(diff) = &self.diff {
            diff.update(cx, |d, cx| d.set_mode(mode, cx));
        }
        cx.notify();
    }
}

impl Render for ChangesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let mut scopes = vec![
            (DiffScope::Thread, "This thread".to_owned()),
            (DiffScope::LastTurn, "Last turn".to_owned()),
            (DiffScope::WorkingTree, "Uncommitted".to_owned()),
        ];
        // Opened from a turn's "changed files" card.
        if let Some(label) = self.turn_label(cx) {
            scopes.push((self.scope.clone(), label));
        }
        let mut segmented = h_flex().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(p.muted);
        for (scope, label) in scopes {
            let active = self.scope == scope;
            segmented = segmented.child(
                div()
                    .id(SharedString::from(format!("scope-{label}")))
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .text_xs()
                    .cursor_pointer()
                    .when(active, |d| {
                        d.bg(p.background)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |d| d.text_color(p.muted_foreground))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_scope(scope.clone(), cx))),
            );
        }
        let stats = self.result.as_ref().map(|r| {
            let added: u32 = r.files.iter().map(|f| f.added).sum();
            let removed: u32 = r.files.iter().map(|f| f.removed).sum();
            (r.files.len(), added, removed)
        });

        let header =
            h_flex()
                .h(px(40.))
                .flex_shrink_0()
                .px_3()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(p.border)
                .child(segmented)
                .child(div().flex_1())
                .when_some(stats, |d, (files, added, removed)| {
                    d.child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .child(div().text_color(p.muted_foreground).child(format!(
                                "{files} file{}",
                                if files == 1 { "" } else { "s" }
                            )))
                            .child(div().text_color(p.success).child(format!("+{added}")))
                            .child(div().text_color(p.danger).child(format!("−{removed}"))),
                    )
                })
                .child(
                    div()
                        .id("split-toggle")
                        .p_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        .child(
                            Icon::new(if self.split {
                                IconName::Rows2
                            } else {
                                IconName::Columns2
                            })
                            .small(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_split(cx))),
                )
                .child(
                    div()
                        .id("refresh-diff")
                        .p_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        .child(Icon::new(IconName::RefreshCw).small())
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                );

        let note = self.result.as_ref().and_then(|r| r.note.clone());
        let body = if let Some(err) = &self.error {
            centered(&p, IconName::CircleAlert, err.clone(), true)
        } else if self.loading && self.diff.is_none() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Spinner::new().color(p.muted_foreground))
                .into_any_element()
        } else if let Some(note) = note {
            centered(&p, IconName::GitCompare, note, false)
        } else if let Some(diff) = &self.diff {
            div()
                .flex_1()
                .min_h_0()
                .child(Diff::new(diff).size_full())
                .into_any_element()
        } else {
            div().flex_1().into_any_element()
        };
        v_flex().size_full().child(header).child(body)
    }
}

fn centered(p: &Palette, icon: IconName, text: String, error: bool) -> gpui_kit::AnyElement {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap_2()
        .px_6()
        .text_sm()
        .text_color(if error { p.danger } else { p.muted_foreground })
        .child(Icon::new(icon).large())
        .child(text)
        .into_any_element()
}
