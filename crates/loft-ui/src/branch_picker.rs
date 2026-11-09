//! Where a new worktree starts: a branch (searchable, the main branch
//! first) and whether to take the local copy or the remote's.

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
use loft_protocol::{BranchList, ProjectId, ThreadId, ThreadWorkspace, WorktreeBase};

pub enum BranchPickerEvent {
    Close,
}

/// One branch name, wherever it exists.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub name: String,
    pub local: bool,
    /// Remotes that have it, `origin` first.
    pub remotes: Vec<String>,
}

/// Local and remote branches merged by name: the main branch, then the
/// checked-out one, then by recency.
pub fn merged(list: &BranchList) -> Vec<Branch> {
    let mut out: Vec<Branch> = Vec::new();
    let mut push = |name: &str, remote: Option<&str>| {
        let ix = match out.iter().position(|b| b.name == name) {
            Some(ix) => ix,
            None => {
                out.push(Branch {
                    name: name.to_owned(),
                    local: false,
                    remotes: Vec::new(),
                });
                out.len() - 1
            }
        };
        match remote {
            None => out[ix].local = true,
            Some(r) if r == "origin" => out[ix].remotes.insert(0, r.to_owned()),
            Some(r) => out[ix].remotes.push(r.to_owned()),
        }
    };
    for name in &list.local {
        push(name, None);
    }
    for rb in &list.remote {
        push(&rb.branch, Some(&rb.remote));
    }
    let rank = |b: &Branch| {
        if list.default_branch.as_deref() == Some(b.name.as_str()) {
            0
        } else if list.current.as_deref() == Some(b.name.as_str()) {
            1
        } else {
            2
        }
    };
    out.sort_by_key(rank); // stable: recency within each rank
    out
}

/// The base a pending worktree will use: the chosen one, else the main
/// branch, from `origin` when it has it (as the daemon decides).
pub fn effective_base(workspace: &ThreadWorkspace, list: &BranchList) -> Option<WorktreeBase> {
    if let ThreadWorkspace::NewWorktree { base: Some(base) } = workspace {
        return Some(base.clone());
    }
    let branch = list.default_branch.clone()?;
    let remote = list
        .remote
        .iter()
        .any(|r| r.remote == "origin" && r.branch == branch)
        .then(|| "origin".to_owned());
    Some(WorktreeBase { branch, remote })
}

pub struct BranchPicker {
    store: Entity<AppStore>,
    thread_id: ThreadId,
    project_id: ProjectId,
    search: Entity<InputState>,
    query: String,
    highlighted: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<BranchPickerEvent> for BranchPicker {}

impl BranchPicker {
    pub fn new(
        store: Entity<AppStore>,
        thread_id: ThreadId,
        project_id: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search branches…"));
        let subs = vec![
            cx.subscribe_in(&search, window, |this, state, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query = state.read(cx).value().to_lowercase();
                    this.highlighted = 0;
                    cx.notify();
                }
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            thread_id,
            project_id,
            search,
            query: String::new(),
            highlighted: 0,
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

    /// Fresh state for an opening; refreshes the branch list.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.query.clear();
        self.highlighted = 0;
        let project = self.project_id.clone();
        self.store.update(cx, |s, cx| s.fetch_branches(project, cx));
    }

    fn rows(&self, cx: &App) -> (Vec<Branch>, Option<WorktreeBase>, BranchList) {
        let store = self.store.read(cx);
        let list = store
            .branches
            .get(&self.project_id)
            .cloned()
            .unwrap_or_default();
        let workspace = store
            .threads
            .get(&self.thread_id)
            .and_then(|t| t.read(cx).model.as_ref())
            .map(|m| m.thread.workspace.clone())
            .unwrap_or_default();
        let base = effective_base(&workspace, &list);
        let rows = merged(&list)
            .into_iter()
            .filter(|b| self.query.is_empty() || b.name.to_lowercase().contains(&self.query))
            .collect();
        (rows, base, list)
    }

    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (rows, base, _) = self.rows(cx);
        let Some(branch) = rows.get(ix) else {
            return;
        };
        // Keep the current source when this branch has it.
        let wants_remote = base.as_ref().is_none_or(|b| b.remote.is_some());
        let remote = match (wants_remote, branch.remotes.first()) {
            (true, Some(r)) => Some(r.clone()),
            _ if branch.local => None,
            (_, r) => r.cloned(),
        };
        let base = WorktreeBase {
            branch: branch.name.clone(),
            remote,
        };
        let t = self.thread_id.clone();
        self.store
            .update(cx, |s, cx| s.set_thread_workspace(t, true, Some(base), cx));
        cx.emit(BranchPickerEvent::Close);
    }

    fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.rows(cx).0.len() as isize;
        if n > 0 {
            self.highlighted = (self.highlighted as isize + delta).rem_euclid(n) as usize;
            self.scroll.scroll_to_item(self.highlighted);
            cx.notify();
        }
    }
}

impl Render for BranchPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::input::{Enter, Escape, MoveDown, MoveUp};
        let p = Palette::of(cx);
        let (rows, base, list) = self.rows(cx);
        if self.highlighted >= rows.len() {
            self.highlighted = rows.len().saturating_sub(1);
        }
        let mut body = v_flex()
            .id("branch-list")
            .max_h(px(300.))
            .p_1()
            .gap_px()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        if rows.is_empty() {
            body = body.child(div().p_3().text_sm().text_color(p.faint_foreground).child(
                if self.query.is_empty() {
                    "Reading branches…"
                } else {
                    "No branches match."
                },
            ));
        }
        for (ix, branch) in rows.iter().enumerate() {
            let active = ix == self.highlighted;
            let chosen = base.as_ref().is_some_and(|b| b.branch == branch.name);
            let is_default = list.default_branch.as_deref() == Some(branch.name.as_str());
            let is_current = list.current.as_deref() == Some(branch.name.as_str());
            body = body.child(
                h_flex()
                    .id(SharedString::from(format!("branch-{ix}")))
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py(px(6.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |d| d.bg(p.selected))
                    .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                    .child(
                        Icon::new(IconName::GitBranch)
                            .xsmall()
                            .text_color(if chosen { p.accent } else { p.muted_foreground }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(chosen, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(branch.name.clone()),
                    )
                    .when(is_default, |d| d.child(badge("default", true, &p)))
                    .when(is_current, |d| d.child(badge("checked out", false, &p)))
                    // Where it exists: this Mac, the remote.
                    .child(
                        Icon::new(IconName::Laptop)
                            .xsmall()
                            .text_color(if branch.local {
                                p.muted_foreground
                            } else {
                                p.border
                            }),
                    )
                    .child(Icon::new(IconName::Cloud).xsmall().text_color(
                        if branch.remotes.is_empty() {
                            p.border
                        } else {
                            p.muted_foreground
                        },
                    ))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.highlighted != ix {
                            this.highlighted = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, cx))),
            );
        }
        v_flex()
            .w(px(360.))
            .rounded(px(12.))
            .overflow_hidden()
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                cx.stop_propagation();
                this.move_by(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                cx.stop_propagation();
                this.move_by(1, cx);
            }))
            .capture_action(cx.listener(|this, _: &Enter, _, cx| {
                cx.stop_propagation();
                let ix = this.highlighted;
                this.pick(ix, cx);
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(BranchPickerEvent::Close);
            }))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        Input::new(&self.search)
                            .prefix(Icon::new(IconName::Search).small())
                            .appearance(false)
                            .small(),
                    ),
            )
            .child(body)
    }
}

fn badge(text: &'static str, accent: bool, p: &Palette) -> AnyElement {
    div()
        .px_1()
        .rounded(px(4.))
        .text_xs()
        .when(accent, |d| d.bg(p.accent_soft).text_color(p.accent))
        .when(!accent, |d| d.bg(p.muted).text_color(p.muted_foreground))
        .child(text)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::RemoteBranch;

    fn list() -> BranchList {
        BranchList {
            local: vec!["feat/x".into(), "main".into()],
            remote: vec![
                RemoteBranch {
                    remote: "upstream".into(),
                    branch: "main".into(),
                },
                RemoteBranch {
                    remote: "origin".into(),
                    branch: "main".into(),
                },
                RemoteBranch {
                    remote: "origin".into(),
                    branch: "release".into(),
                },
            ],
            default_branch: Some("main".into()),
            current: Some("feat/x".into()),
        }
    }

    #[test]
    fn merges_by_name_main_first() {
        let rows = merged(&list());
        let names: Vec<&str> = rows.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["main", "feat/x", "release"]);
        assert!(rows[0].local);
        assert_eq!(rows[0].remotes, ["origin", "upstream"]);
        assert!(!rows[2].local);
    }

    #[test]
    fn default_base_prefers_origin() {
        let base = effective_base(&ThreadWorkspace::NewWorktree { base: None }, &list()).unwrap();
        assert_eq!(base.branch, "main");
        assert_eq!(base.remote.as_deref(), Some("origin"));
        let chosen = WorktreeBase {
            branch: "feat/x".into(),
            remote: None,
        };
        let ws = ThreadWorkspace::NewWorktree {
            base: Some(chosen.clone()),
        };
        assert_eq!(effective_base(&ws, &list()), Some(chosen));
    }
}
