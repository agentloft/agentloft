//! Settings ▸ Projects: where new projects go, and each project's name,
//! pin and removal.

use super::{Field, SettingsView, row, text_input};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::ProjectId;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ProjectsPage {
    /// A name field per project (made when first shown).
    names: HashMap<ProjectId, (Entity<InputState>, Subscription)>,
    /// The project whose Remove is asking "sure?".
    confirming: Option<ProjectId>,
}

impl SettingsView {
    pub(super) fn projects_page(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (projects, threads, pinned) = {
            let store = self.store.read(cx);
            let projects: Vec<loft_protocol::Project> = store
                .workspace
                .projects
                .iter()
                .filter(|p| !store.is_chats(p))
                .cloned()
                .collect();
            let threads: HashMap<ProjectId, usize> = projects
                .iter()
                .map(|p| {
                    let n = store
                        .workspace
                        .threads
                        .iter()
                        .filter(|t| {
                            t.project_id == p.id
                                && t.status != loft_protocol::ThreadStatus::Archived
                        })
                        .count();
                    (p.id.clone(), n)
                })
                .collect();
            (projects, threads, store.pinned_projects.clone())
        };
        // Name fields for projects shown for the first time; gone ones go.
        self.projects
            .names
            .retain(|id, _| projects.iter().any(|p| &p.id == id));
        for project in &projects {
            if self.projects.names.contains_key(&project.id) {
                continue;
            }
            let name = project.name.clone();
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx);
                s.set_value(name, window, cx);
                s
            });
            let id = project.id.clone();
            let sub = cx.subscribe_in(&input, window, move |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let text = state.read(cx).value().trim().to_owned();
                    let old = this
                        .store
                        .read(cx)
                        .workspace
                        .projects
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.name.clone());
                    if !text.is_empty() && old.as_deref() != Some(text.as_str()) {
                        let id = id.clone();
                        this.store
                            .update(cx, |s, cx| s.rename_project(id, text, cx));
                    }
                }
            });
            self.projects.names.insert(project.id.clone(), (input, sub));
        }

        let mut page = v_flex().child(row(
            "New projects start in",
            "Where Add Project starts browsing, and where new and cloned projects go. Empty: your home folder.",
            text_input(&self.input(Field::ProjectsDir)),
            p,
        ));
        page = page.child(
            div()
                .pt_6()
                .pb_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.muted_foreground)
                .child("YOUR PROJECTS"),
        );
        if projects.is_empty() {
            page = page.child(
                div()
                    .py_3()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child("No projects yet. Add one with ⌘O."),
            );
        }
        for project in &projects {
            let count = threads.get(&project.id).copied().unwrap_or(0);
            page =
                page.child(self.project_card(project, count, pinned.contains(&project.id), p, cx));
        }
        page.into_any_element()
    }

    fn project_card(
        &self,
        project: &loft_protocol::Project,
        threads: usize,
        pinned: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = project.id.clone();
        let confirming = self.projects.confirming.as_ref() == Some(&id);
        let name_input = self.projects.names.get(&id).map(|(i, _)| i.clone());
        let path = std::path::PathBuf::from(&project.root_path);
        let shown_path = crate::workspace::tilde_path_str(&project.root_path);
        let line = |label: &str, hint: String, control: AnyElement| {
            h_flex()
                .gap_6()
                .items_center()
                .py_2()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(label.to_owned()),
                        )
                        .child(div().text_xs().text_color(p.muted_foreground).child(hint)),
                )
                .child(div().flex_shrink_0().child(control))
        };
        let pin_id = id.clone();
        let remove_id = id.clone();
        let (cancel_id, confirm_id) = (id.clone(), id.clone());
        let what = match threads {
            0 => "its entry".to_owned(),
            1 => "its 1 thread".to_owned(),
            n => format!("its {n} threads"),
        };
        v_flex()
            .my_2()
            .px_4()
            .py_2()
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            // Header: name and folder.
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .py_2()
                    .border_b_1()
                    .border_color(p.border.opacity(0.6))
                    .child(Icon::new(IconName::FolderOpen).small().text_color(p.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(project.name.clone()),
                    )
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("reveal-{id}")))
                            .gap_1()
                            .items_center()
                            .max_w(px(320.))
                            .px_2()
                            .py_0p5()
                            .rounded(px(6.))
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .cursor_pointer()
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                            .child(div().truncate().child(shown_path))
                            .child(Icon::new(IconName::FolderOpen).xsmall())
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    ),
            )
            .child(line(
                "Name",
                "How it shows in the sidebar and pickers.".into(),
                match name_input {
                    Some(input) => div()
                        .w(px(220.))
                        .child(Input::new(&input).small())
                        .into_any_element(),
                    None => div().into_any_element(),
                },
            ))
            .child(line(
                "Pin",
                "Pinned projects come first in the sidebar and project pickers.".into(),
                Switch::new(SharedString::from(format!("pin-{id}")))
                    .checked(pinned)
                    .on_click(cx.listener(move |this, _: &bool, _, cx| {
                        let id = pin_id.clone();
                        this.store.update(cx, |s, cx| s.toggle_pinned(id, cx));
                        cx.notify();
                    }))
                    .into_any_element(),
            ))
            .child(line(
                "Remove project",
                format!("Removes the project from Agentloft and deletes {what}. Files on disk are not touched."),
                if confirming {
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("remove-cancel-{id}")))
                                .small()
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.projects.confirming.as_ref() == Some(&cancel_id) {
                                        this.projects.confirming = None;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("remove-yes-{id}")))
                                .small()
                                .danger()
                                .icon(IconName::Trash)
                                .label("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.projects.confirming = None;
                                    let id = confirm_id.clone();
                                    this.store.update(cx, |s, cx| s.remove_project(id, cx));
                                    cx.notify();
                                })),
                        )
                        .into_any_element()
                } else {
                    Button::new(SharedString::from(format!("remove-{id}")))
                        .small()
                        .outline()
                        .icon(IconName::Trash)
                        .label("Remove project")
                        .text_color(p.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.projects.confirming = Some(remove_id.clone());
                            cx.notify();
                        }))
                        .into_any_element()
                },
            ))
            .when(confirming, |d| {
                d.child(
                    div()
                        .pb_2()
                        .text_xs()
                        .text_color(p.danger)
                        .child(format!(
                            "Remove “{}” and delete {what}? This can't be undone.",
                            project.name
                        )),
                )
            })
            .into_any_element()
    }
}
