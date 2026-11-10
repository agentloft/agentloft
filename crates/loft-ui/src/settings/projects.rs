//! Settings ▸ Projects: where projects go, then one row per project; a row
//! opens that project's page (name, pin, defaults for new threads,
//! removal).

use super::{Field, SettingsView, row, text_input};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{HarnessId, ProjectDefaults, ProjectId};

#[derive(Default)]
pub(super) struct ProjectsPage {
    /// The project whose page is open (else the list).
    open: Option<ProjectId>,
    /// Its name field.
    name: Option<(ProjectId, Entity<InputState>, Subscription)>,
    /// Remove is asking "sure?".
    confirming: bool,
}

impl SettingsView {
    /// Open a project's page (the sidebar's "Project Settings…").
    pub(super) fn open_project(&mut self, id: ProjectId) {
        self.projects.open = Some(id);
        self.projects.confirming = false;
    }

    pub(super) fn projects_page(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.projects.open.clone().and_then(|id| {
            self.store
                .read(cx)
                .workspace
                .projects
                .iter()
                .find(|p| p.id == id)
                .cloned()
        });
        match open {
            Some(project) => self.project_page(project, p, window, cx),
            None => {
                self.projects.open = None;
                self.project_list(p, cx)
            }
        }
    }

    fn project_list(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let projects: Vec<loft_protocol::Project> = {
            let store = self.store.read(cx);
            let mut list: Vec<_> = store
                .workspace
                .projects
                .iter()
                .filter(|p| !store.is_chats(p))
                .cloned()
                .collect();
            list.sort_by_key(|p| p.name.to_lowercase());
            list
        };
        let mut rows = v_flex()
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            .overflow_hidden();
        for (ix, project) in projects.iter().enumerate() {
            let id = project.id.clone();
            rows = rows.child(
                h_flex()
                    .id(SharedString::from(format!("project-row-{id}")))
                    .gap_3()
                    .items_center()
                    .px_4()
                    .py_2p5()
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .when(ix > 0, |d| {
                        d.border_t_1().border_color(p.border.opacity(0.6))
                    })
                    .child(crate::project_icon::badge(project, 24., p))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(project.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(p.muted_foreground)
                                    .child(crate::workspace::tilde_path_str(&project.root_path)),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .small()
                            .text_color(p.muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_project(id.clone());
                        cx.notify();
                    })),
            );
        }
        if projects.is_empty() {
            rows = rows.child(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child(if crate::host::IS_WEB {
                        "No projects yet. Add one in the desktop app.".to_owned()
                    } else {
                        format!(
                            "No projects yet. Add one with {}.",
                            crate::keymap::display("secondary-o")
                        )
                    }),
            );
        }
        v_flex()
            // Which folders projects come from is the desktop app's to set.
            .when(!crate::host::IS_WEB, |d| {
                d.child(row(
                    "New projects go in",
                    "Where a project made from a name is created (a folder named after it). Empty: ~/.agentloft/projects.",
                    text_input(&self.input(Field::NewProjectsDir)),
                    p,
                ))
                .child(row(
                    "Add project starts in",
                    "Where Add Project's folder browser opens, and where clones go. Empty: your home folder.",
                    text_input(&self.input(Field::ProjectsDir)),
                    p,
                ))
            })
            .child(
                div()
                    .pt_6()
                    .pb_2()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .child("Choose a project to manage its name, defaults for new threads, and removal."),
            )
            .child(rows)
            .into_any_element()
    }

    fn project_page(
        &mut self,
        project: loft_protocol::Project,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = project.id.clone();
        // A name field for this project.
        if self.projects.name.as_ref().is_none_or(|(n, _, _)| *n != id) {
            let name = project.name.clone();
            let input = cx.new(|cx| {
                let mut s = InputState::new(window, cx);
                s.set_value(name, window, cx);
                s
            });
            let rid = id.clone();
            let sub = cx.subscribe_in(&input, window, move |this, state, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let text = state.read(cx).value().trim().to_owned();
                    let old = this
                        .store
                        .read(cx)
                        .workspace
                        .projects
                        .iter()
                        .find(|p| p.id == rid)
                        .map(|p| p.name.clone());
                    if !text.is_empty() && old.as_deref() != Some(text.as_str()) {
                        let id = rid.clone();
                        this.store
                            .update(cx, |s, cx| s.rename_project(id, text, cx));
                    }
                }
            });
            self.projects.name = Some((id.clone(), input, sub));
        }
        let name_input = self.projects.name.as_ref().map(|(_, i, _)| i.clone());
        let (pinned, threads, agents) = {
            let store = self.store.read(cx);
            let threads = store
                .workspace
                .threads
                .iter()
                .filter(|t| t.project_id == id && t.status != loft_protocol::ThreadStatus::Archived)
                .count();
            (
                store.pinned_projects.contains(&id),
                threads,
                store.available_harnesses(),
            )
        };
        // Models of the project's agent, for its menus.
        if let Some(agent) = project.defaults.agent.clone() {
            self.store.update(cx, |s, cx| s.ensure_models(agent, cx));
        }
        let card = |title: &str, body: gpui_kit::Div| {
            v_flex()
                .gap_2()
                .pt_6()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(
                    body.px_4()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(p.border),
                )
        };
        let what = match threads {
            0 => "the project entry".to_owned(),
            1 => "the project entry and its thread".to_owned(),
            n => format!("the project entry and its {n} threads"),
        };
        let remove = if self.projects.confirming {
            let (cancel, yes) = (id.clone(), id.clone());
            h_flex()
                .gap_2()
                .child(
                    Button::new("remove-cancel")
                        .small()
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let _ = &cancel;
                            this.projects.confirming = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("remove-yes")
                        .small()
                        .danger()
                        .icon(IconName::Trash)
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.projects.confirming = false;
                            this.projects.open = None;
                            let id = yes.clone();
                            this.store.update(cx, |s, cx| s.remove_project(id, cx));
                            cx.notify();
                        })),
                )
                .into_any_element()
        } else {
            Button::new("remove")
                .small()
                .outline()
                .icon(IconName::Trash)
                .label("Remove project")
                .text_color(p.danger)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.projects.confirming = true;
                    cx.notify();
                }))
                .into_any_element()
        };
        let pin_id = id.clone();
        let icon_hint = if project.icon.is_some() {
            "A chosen image.".to_owned()
        } else {
            match crate::project_icon::favicon(std::path::Path::new(&project.root_path)) {
                Some(f) => format!(
                    "Automatic: {}",
                    f.strip_prefix(&project.root_path)
                        .map(|r| r.display().to_string())
                        .unwrap_or_default()
                ),
                None => "Automatic: no favicon found, so its initials.".into(),
            }
        };
        v_flex()
            // Back to the list.
            .child(
                h_flex()
                    .id("projects-back")
                    .gap_1()
                    .items_center()
                    .mb_2()
                    .text_sm()
                    .text_color(p.muted_foreground)
                    .cursor_pointer()
                    .hover(|d| d.text_color(p.foreground))
                    .child(Icon::new(IconName::ChevronLeft).small())
                    .child("All projects")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.projects.open = None;
                        this.projects.confirming = false;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(project.name.clone()),
                    )
                    .child(
                        h_flex()
                            .id("project-reveal")
                            .gap_1()
                            .items_center()
                            .px_2()
                            .py_0p5()
                            .rounded(px(6.))
                            .text_xs()
                            .text_color(p.muted_foreground)
                            .cursor_pointer()
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                            .child(crate::workspace::tilde_path_str(&project.root_path))
                            .child(Icon::new(IconName::FolderOpen).xsmall())
                            .on_click({
                                let path = std::path::PathBuf::from(&project.root_path);
                                move |_, _, cx| cx.reveal_path(&path)
                            }),
                    ),
            )
            .child(card(
                "Project",
                v_flex()
                    .child(row(
                        "Name",
                        "How it shows in the sidebar and pickers.",
                        match name_input {
                            Some(input) => div()
                                .w(px(260.))
                                .child(Input::new(&input).small())
                                .into_any_element(),
                            None => div().into_any_element(),
                        },
                        p,
                    ))
                    .child(row(
                        "Project icon",
                        &icon_hint,
                        self.icon_controls(&project, p, cx),
                        p,
                    ))
                    .child(row(
                        "Pin",
                        "Pinned projects come first in the sidebar and project pickers.",
                        Switch::new("project-pin")
                            .checked(pinned)
                            .on_click(cx.listener(move |this, _: &bool, _, cx| {
                                let id = pin_id.clone();
                                this.store.update(cx, |s, cx| s.toggle_pinned(id, cx));
                                cx.notify();
                            }))
                            .into_any_element(),
                        p,
                    )),
            ))
            .child(card(
                "New threads",
                v_flex()
                    .child(row(
                        "Agent and model",
                        "What new threads in this project start with. Default: your last choice and each agent's defaults.",
                        self.agent_menus(&project, &agents, p, cx),
                        p,
                    ))
                    .child(row(
                        "Workspace",
                        "Where new threads in this project work.",
                        self.workspace_menu(&project, p, cx),
                        p,
                    )),
            ))
            .child(card(
                "Danger",
                v_flex().child(row(
                    "Remove project",
                    &format!("Deletes {what}. Files on disk are not touched."),
                    remove,
                    p,
                )),
            ))
            .into_any_element()
    }

    /// The icon, "Choose file…", and "Reset" once one was chosen.
    fn icon_controls(
        &self,
        project: &loft_protocol::Project,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (pick_id, reset_id) = (project.id.clone(), project.id.clone());
        h_flex()
            .gap_2()
            .items_center()
            .child(crate::project_icon::badge(project, 26., p))
            // Picking a file needs this computer's file dialog.
            .when(!crate::host::IS_WEB, |d| {
                d.child(
                    Button::new("project-icon-file")
                        .small()
                        .outline()
                        .label("Choose file…")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                                files: true,
                                directories: false,
                                multiple: false,
                                prompt: Some("Use as icon".into()),
                            });
                            let (store, id) = (this.store.clone(), pick_id.clone());
                            cx.spawn(async move |_, cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    store
                                        .update(cx, |s, cx| s.set_project_icon(id, Some(path), cx));
                                }
                            })
                            .detach();
                        })),
                )
            })
            .when(project.icon.is_some(), |d| {
                d.child(
                    Button::new("project-icon-reset")
                        .small()
                        .ghost()
                        .label("Reset")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = reset_id.clone();
                            this.store
                                .update(cx, |s, cx| s.set_project_icon(id, None, cx));
                        })),
                )
            })
            .into_any_element()
    }

    /// Set the open project's defaults (via the daemon).
    fn set_defaults(
        &mut self,
        project: &loft_protocol::Project,
        change: impl FnOnce(&mut ProjectDefaults),
        cx: &mut Context<Self>,
    ) {
        let mut d = project.defaults.clone();
        change(&mut d);
        let id = project.id.clone();
        self.store
            .update(cx, |s, cx| s.set_project_defaults(id, d, cx));
    }

    /// Agent, then its model, then the model's effort.
    fn agent_menus(
        &self,
        project: &loft_protocol::Project,
        agents: &[(&'static str, &'static str)],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = p;
        let d = project.defaults.clone();
        let view = cx.entity();
        let models: Vec<loft_protocol::ModelInfo> = d
            .agent
            .as_ref()
            .and_then(|a| self.store.read(cx).models.get(a).cloned())
            .unwrap_or_default();
        let agent_name = d
            .agent
            .as_ref()
            .and_then(|a| agents.iter().find(|(id, _)| *id == a.as_str()))
            .map(|(_, n)| (*n).to_owned())
            .unwrap_or_else(|| "Default agent".into());
        let agent_btn = {
            let (view, project, agents) = (view.clone(), project.clone(), agents.to_vec());
            let current = d.agent.clone();
            Button::new("project-agent")
                .small()
                .outline()
                .label(agent_name)
                .dropdown_menu(move |mut menu, _, _| {
                    let (v, pr) = (view.clone(), project.clone());
                    menu = menu
                        .item(
                            PopupMenuItem::new("Default")
                                .checked(current.is_none())
                                .on_click(move |_, _, cx| {
                                    v.update(cx, |this, cx| {
                                        this.set_defaults(
                                            &pr,
                                            |d| {
                                                *d = ProjectDefaults {
                                                    worktree: d.worktree,
                                                    ..Default::default()
                                                }
                                            },
                                            cx,
                                        )
                                    })
                                }),
                        )
                        .separator();
                    for (id, name) in &agents {
                        let (v, pr, id) = (view.clone(), project.clone(), *id);
                        let checked = current.as_ref().is_some_and(|a| a.as_str() == id);
                        menu = menu.item(PopupMenuItem::new(*name).checked(checked).on_click(
                            move |_, _, cx| {
                                v.update(cx, |this, cx| {
                                    this.set_defaults(
                                        &pr,
                                        |d| {
                                            // Another agent: its own models.
                                            if d.agent.as_ref().map(HarnessId::as_str) != Some(id) {
                                                d.model = None;
                                                d.effort = None;
                                            }
                                            d.agent = Some(HarnessId(id.into()));
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
        let mut out = h_flex().gap_2().child(agent_btn);
        if d.agent.is_some() {
            let model = d.model.as_ref().and_then(|m| {
                models
                    .iter()
                    .find(|x| &x.id == m || x.resolved_id.as_deref() == Some(m.as_str()))
            });
            let model_label = match (&d.model, model) {
                (Some(_), Some(m)) => m.display_name.clone(),
                (Some(m), None) => crate::workspace::pretty_model(m),
                (None, _) => "Agent's default model".into(),
            };
            let model_btn = {
                let (view, project) = (view.clone(), project.clone());
                let options: Vec<(String, String)> = models
                    .iter()
                    .map(|m| (m.id.clone(), m.display_name.clone()))
                    .collect();
                let current = d.model.clone();
                Button::new("project-model")
                    .small()
                    .outline()
                    .label(model_label)
                    .dropdown_menu(move |mut menu, _, _| {
                        let (v, pr) = (view.clone(), project.clone());
                        menu = menu
                            .item(
                                PopupMenuItem::new("Agent's default")
                                    .checked(current.is_none())
                                    .on_click(move |_, _, cx| {
                                        v.update(cx, |this, cx| {
                                            this.set_defaults(
                                                &pr,
                                                |d| {
                                                    d.model = None;
                                                    d.effort = None;
                                                },
                                                cx,
                                            )
                                        })
                                    }),
                            )
                            .separator();
                        for (mid, name) in &options {
                            let (v, pr, mid) = (view.clone(), project.clone(), mid.clone());
                            let checked = current.as_deref() == Some(mid.as_str());
                            menu = menu.item(
                                PopupMenuItem::new(name.clone()).checked(checked).on_click(
                                    move |_, _, cx| {
                                        let mid = mid.clone();
                                        v.update(cx, |this, cx| {
                                            this.set_defaults(&pr, |d| d.model = Some(mid), cx)
                                        })
                                    },
                                ),
                            );
                        }
                        menu
                    })
            };
            out = out.child(model_btn);
            // Its efforts, when it has any.
            let efforts = model.map(|m| m.efforts.clone()).unwrap_or_default();
            if !efforts.is_empty() {
                let label = d
                    .effort
                    .as_deref()
                    .map(|e| crate::model_picker::effort_label(e).to_owned())
                    .unwrap_or_else(|| "Default effort".into());
                let (view, project) = (view.clone(), project.clone());
                let current = d.effort.clone();
                out = out.child(
                    Button::new("project-effort")
                        .small()
                        .outline()
                        .label(label)
                        .dropdown_menu(move |mut menu, _, _| {
                            let (v, pr) = (view.clone(), project.clone());
                            menu = menu
                                .item(
                                    PopupMenuItem::new("Model's default")
                                        .checked(current.is_none())
                                        .on_click(move |_, _, cx| {
                                            v.update(cx, |this, cx| {
                                                this.set_defaults(&pr, |d| d.effort = None, cx)
                                            })
                                        }),
                                )
                                .separator();
                            for e in &efforts {
                                let (v, pr, e) = (view.clone(), project.clone(), e.clone());
                                let checked = current.as_deref() == Some(e.as_str());
                                menu = menu.item(
                                    PopupMenuItem::new(
                                        crate::model_picker::effort_label(&e).to_owned(),
                                    )
                                    .checked(checked)
                                    .on_click(
                                        move |_, _, cx| {
                                            let e = e.clone();
                                            v.update(cx, |this, cx| {
                                                this.set_defaults(&pr, |d| d.effort = Some(e), cx)
                                            })
                                        },
                                    ),
                                );
                            }
                            menu
                        }),
                );
            }
        }
        out.into_any_element()
    }

    fn workspace_menu(
        &self,
        project: &loft_protocol::Project,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = p;
        let current = project.defaults.worktree;
        let label = match current {
            None => "Default (Settings ▸ Worktrees)",
            Some(false) => "Current checkout",
            Some(true) => "New worktree",
        };
        let (view, project) = (cx.entity(), project.clone());
        Button::new("project-workspace")
            .small()
            .outline()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (value, name) in [
                    (None, "Default (Settings ▸ Worktrees)"),
                    (Some(false), "Current checkout"),
                    (Some(true), "New worktree"),
                ] {
                    let (v, pr) = (view.clone(), project.clone());
                    menu = menu.item(PopupMenuItem::new(name).checked(current == value).on_click(
                        move |_, _, cx| {
                            v.update(cx, |this, cx| {
                                this.set_defaults(&pr, |d| d.worktree = value, cx)
                            })
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }
}
