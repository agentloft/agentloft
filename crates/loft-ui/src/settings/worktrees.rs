//! Settings ▸ Worktrees, cleaning up: what happens to a worktree whose work
//! is already in its base branch (`[worktrees] cleanup`, `delete_branch`,
//! `delete_remote_branch`, `archive_after_cleanup`), and the worktrees and
//! branches left over from threads that are gone or archived, to remove
//! the ones whose work is in the main branch.

use super::{SettingsView, row, segmented, web::heading};
use crate::press::ClaimsPress as _;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};
use loft_config::SettingValue;
use loft_protocol::WorktreeLeftover;

impl SettingsView {
    /// The cleanup rows.
    pub(super) fn worktree_cleanup(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let cfg = self.config.worktrees.clone();
        let mode = segmented(
            "wt-cleanup",
            &[
                ("off", "Off"),
                ("offer", "Offer"),
                ("auto", "Automatically"),
            ],
            &cfg.cleanup,
            p,
            cx.listener(|this, value: &&str, _, cx| {
                // Offer is the default.
                let value = if *value == "offer" {
                    SettingValue::Unset
                } else {
                    SettingValue::Str((*value).to_owned())
                };
                this.write(&["worktrees", "cleanup"], value, true, cx);
            }),
        );
        let on = cfg.cleans_up();
        // A switch whose default is `default`: that value is left unset.
        let switch = |id: &'static str, key: &'static str, checked: bool, default: bool| {
            Switch::new(id)
                .cursor_pointer()
                .checked(checked)
                .disabled(!on)
                .on_click(cx.listener(move |this, value: &bool, _, cx| {
                    let value = if *value == default {
                        SettingValue::Unset
                    } else {
                        SettingValue::Bool(*value)
                    };
                    this.write(&["worktrees", key], value, true, cx);
                }))
                .into_any_element()
        };
        v_flex()
            .child(heading(
                "Cleaning up",
                "A worktree whose work is all in its base branch (merged back, or its pull request merged) and has nothing uncommitted can go without losing anything. The thread stays: its next message makes a new worktree, in the same folder, from the base as it is then.",
                p,
            ))
            .child(row(
                "Clean up merged worktrees",
                "Offer: the thread offers to. Automatically: once nothing runs there. Off: worktrees stay until their thread is archived.",
                mode,
                p,
            ))
            .child(row(
                "Delete the branch too",
                "Also when archiving or deleting a thread. Its last commit stays on the thread, to restore it.",
                switch(
                    "wt-delete-branch",
                    "delete_branch",
                    cfg.delete_branch,
                    true,
                ),
                p,
            ))
            .child(row(
                "Delete the remote branch",
                "Once its pull request is merged and the remote branch is where the thread pushed it. GitHub can also do this for you.",
                switch(
                    "wt-delete-remote",
                    "delete_remote_branch",
                    cfg.delete_remote_branch,
                    false,
                ),
                p,
            ))
            .child(row(
                "Archive the thread after cleaning up",
                "Off: the thread stays in the sidebar.",
                switch(
                    "wt-archive-after",
                    "archive_after_cleanup",
                    cfg.archive_after_cleanup,
                    false,
                ),
                p,
            ))
            .into_any_element()
    }

    /// Worktrees and branches no active thread works in.
    pub(super) fn worktree_leftovers(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.leftovers_asked {
            self.leftovers_asked = true;
            self.store
                .update(cx, |s, cx| s.fetch_worktree_leftovers(cx));
        }
        let store = self.store.read(cx);
        let leftovers = store.worktree_leftovers.clone();
        let project_name = |id: &loft_protocol::ProjectId| {
            store
                .workspace
                .projects
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        let mut page = v_flex();
        let landed: Vec<WorktreeLeftover> = leftovers
            .iter()
            .flatten()
            .filter(|l| l.landed)
            .cloned()
            .collect();
        let s = self.store.clone();
        let refresh = Button::new("wt-leftovers-refresh")
            .small()
            .ghost()
            .cursor_pointer()
            .icon(IconName::RefreshCw)
            .disabled(leftovers.is_none())
            .on_click(move |_, _, cx| s.update(cx, |s, cx| s.fetch_worktree_leftovers(cx)));
        let s = self.store.clone();
        let count = landed.len();
        let clean_all = Button::new("wt-leftovers-clean")
            .small()
            .primary()
            .cursor_pointer()
            .label(format!("Remove {count} merged"))
            .disabled(count == 0)
            .on_click(move |_, _, cx| {
                let landed = landed.clone();
                s.update(cx, |s, cx| s.remove_worktree_leftovers(landed, cx));
            });
        page = page.child(
            h_flex()
                .items_end()
                .gap_2()
                .child(div().flex_1().min_w_0().child(heading(
                    "Left over",
                    "Worktrees and branches no active thread works in: from deleted threads, archived ones, or before cleaning up existed. Only those whose work is in the main branch can be removed here.",
                    p,
                )))
                .child(div().flex_none().child(refresh))
                .child(div().flex_none().child(clean_all)),
        );
        let Some(leftovers) = leftovers else {
            return page
                .child(
                    div()
                        .py_3()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Looking…"),
                )
                .into_any_element();
        };
        if leftovers.is_empty() {
            return page
                .child(
                    div()
                        .py_3()
                        .text_sm()
                        .text_color(p.muted_foreground)
                        .child("Nothing left over."),
                )
                .into_any_element();
        }
        let mut project = None;
        for (ix, l) in leftovers.iter().enumerate() {
            if project.as_ref() != Some(&l.project_id) {
                project = Some(l.project_id.clone());
                page = page.child(
                    div()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(p.muted_foreground)
                        .child(project_name(&l.project_id)),
                );
            }
            page = page.child(leftover_row(ix, l, &self.store, p));
        }
        page.into_any_element()
    }
}

/// One leftover: what it is, whether its work is in the main branch, and a
/// way to remove it if so.
fn leftover_row(
    ix: usize,
    l: &WorktreeLeftover,
    store: &gpui_kit::Entity<crate::store::AppStore>,
    p: &Palette,
) -> AnyElement {
    let folder = l.path.as_ref().map(|path| {
        std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone())
    });
    let (icon, name, what) = match (&l.branch, &folder) {
        (Some(branch), Some(_)) => (IconName::FolderGit2, branch.clone(), "worktree"),
        (Some(branch), None) => (IconName::GitBranch, branch.clone(), "branch"),
        (None, Some(folder)) => (IconName::FolderGit2, folder.clone(), "worktree"),
        (None, None) => (IconName::GitBranch, String::new(), "branch"),
    };
    let (state, color) = if l.dirty {
        ("uncommitted changes".to_owned(), p.warning)
    } else if l.landed {
        ("in the main branch".to_owned(), p.success)
    } else {
        (
            format!(
                "{} commit{} not in the main branch",
                l.ahead,
                if l.ahead == 1 { "" } else { "s" }
            ),
            p.warning,
        )
    };
    let archived = l.thread_id.is_some();
    let remove = l.landed.then(|| {
        let (store, one) = (store.clone(), l.clone());
        div()
            .id(SharedString::from(format!("wt-leftover-remove-{ix}")))
            .flex_none()
            .p_1()
            .rounded(px(5.))
            .cursor_pointer()
            .text_color(p.muted_foreground)
            .hover(|d| d.bg(p.hover).text_color(p.danger))
            .claims_press()
            .child(Icon::new(IconName::Trash).xsmall())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let one = vec![one.clone()];
                store.update(cx, |s, cx| s.remove_worktree_leftovers(one, cx));
            })
    });
    h_flex()
        .gap_2()
        .items_center()
        .py_1p5()
        .border_b_1()
        .border_color(p.border.opacity(0.6))
        .child(
            Icon::new(icon)
                .xsmall()
                .text_color(p.muted_foreground)
                .into_any_element(),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_family(p.mono.clone())
                .child(name),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(p.faint_foreground)
                .child(if archived {
                    format!("{what} · archived thread")
                } else {
                    what.to_owned()
                }),
        )
        .child(div().flex_none().text_xs().text_color(color).child(state))
        .when_some(remove, |d, remove| d.child(remove))
        .when(!l.landed, |d| d.child(div().flex_none().w(px(22.))))
        .into_any_element()
}
