//! Command palette (⌘K): jump to any thread across projects, or run an
//! action — app-wide or on the current thread — with its shortcut shown.

use crate::panel::Tab;
use crate::workspace::{
    AddProject, Interrupt, NewTab, NewThread, NewWindow, OpenSettings, ShowBrowser, ShowChanges,
    ShowFiles, ShowTerminal, TogglePanel, ToggleSidebar, ToggleTheme, WorkspaceView, harness_name,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IndexPath, WindowExt as _};
use gpui_kit::{Action, App, AppContext as _, Context, Entity, ParentElement as _, Window, px};
use loft_protocol::{HarnessId, ThreadStatus, ThreadWorkspace};
use std::rc::Rc;

type Run = Rc<dyn Fn(&mut WorkspaceView, &mut Window, &mut Context<WorkspaceView>)>;

struct Entry {
    item: CommandItem,
    run: Run,
}

fn entry(
    label: impl Into<String>,
    icon: IconName,
    action: Option<Box<dyn Action>>,
    run: impl Fn(&mut WorkspaceView, &mut Window, &mut Context<WorkspaceView>) + 'static,
) -> Entry {
    let mut item = CommandItem::new().label(label.into()).icon(icon);
    // The action is only for the shortcut hint; `run` does the work.
    if let Some(action) = action {
        item = item.action(action);
    }
    Entry {
        item,
        run: Rc::new(run),
    }
}

/// Called from the workspace's own handlers (while it's being updated), so
/// it reads the store, not the view.
pub fn open(
    view: Entity<WorkspaceView>,
    store: &Entity<crate::store::AppStore>,
    window: &mut Window,
    cx: &mut App,
) {
    let groups = Rc::new(build(store, cx));
    let state = cx.new(|cx| CommandState::new(window, cx));
    let focus = state.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let (groups, view) = (groups.clone(), view.clone());
        let mut command = Command::new(&state)
            .placeholder("Search threads and commands…")
            .max_h(px(420.));
        for (label, entries) in groups.iter() {
            command = command.group(
                CommandGroup::new()
                    .label(label.clone())
                    .items(entries.iter().map(|e| e.item.clone())),
            );
        }
        dialog
            .w(px(600.))
            .margin_top(px(96.))
            .close_button(false)
            .child(command.on_confirm(move |ix: IndexPath, window, cx| {
                let Some(run) = groups
                    .get(ix.section)
                    .and_then(|(_, entries)| entries.get(ix.row))
                    .map(|e| e.run.clone())
                else {
                    return;
                };
                window.close_dialog(cx);
                view.update(cx, |v, cx| run(v, window, cx));
            }))
    });
    focus.update(cx, |s, cx| s.focus(window, cx));
}

fn build(store: &Entity<crate::store::AppStore>, cx: &App) -> Vec<(String, Vec<Entry>)> {
    let store = store.read(cx);
    let selected = store
        .selected
        .as_ref()
        .and_then(|id| store.workspace.threads.iter().find(|t| &t.id == id))
        .cloned();
    let mut groups = Vec::new();

    // Threads, most recent first (the workspace keeps that order).
    let threads: Vec<Entry> = store
        .workspace
        .threads
        .iter()
        .filter(|t| t.status != ThreadStatus::Archived)
        .map(|t| {
            let project = store
                .workspace
                .projects
                .iter()
                .find(|p| p.id == t.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let icon = if t.parent.is_some() {
                IconName::GitFork
            } else if t.workspace.branch().is_some() {
                IconName::GitBranch
            } else {
                IconName::Sparkles
            };
            let mut keywords = vec![project.clone(), harness_name(&t.harness).to_owned()];
            keywords.extend(t.workspace.branch().map(str::to_owned));
            let id = t.id.clone();
            Entry {
                item: CommandItem::new()
                    .label(format!("{} — {project}", t.title))
                    .icon(icon)
                    .keywords(keywords),
                run: Rc::new(move |v, _, cx| {
                    let id = id.clone();
                    v.store.update(cx, |s, cx| s.select_thread(id, cx));
                }),
            }
        })
        .collect();
    if !threads.is_empty() {
        groups.push(("Threads".to_owned(), threads));
    }

    if let Some(t) = selected {
        let running = matches!(
            t.status,
            ThreadStatus::Running | ThreadStatus::WaitingApproval
        );
        let mut items = Vec::new();
        if running {
            items.push(entry(
                "Stop turn",
                IconName::Square,
                Some(Box::new(Interrupt)),
                |v, _, cx| v.interrupt(cx),
            ));
        }
        for (h, name) in [
            (HarnessId::CLAUDE, "Claude Code"),
            (HarnessId::CODEX, "Codex"),
        ] {
            let id = t.id.clone();
            items.push(entry(
                format!("Fork with {name}"),
                IconName::GitFork,
                None,
                move |v, _, cx| {
                    let id = id.clone();
                    v.store
                        .update(cx, |s, cx| s.fork_thread(id, HarnessId(h.into()), cx));
                },
            ));
        }
        let id = t.id.clone();
        items.push(entry(
            "Open in new tab",
            IconName::SquarePen,
            None,
            move |_, window, cx| {
                let tab = crate::windows::Placement::TabOf(window.window_handle());
                crate::windows::open(Some(id.clone()), tab, cx);
            },
        ));
        if let ThreadWorkspace::Worktree { base, .. } = &t.workspace
            && !running
        {
            let id = t.id.clone();
            items.push(entry(
                format!("Merge into {base}"),
                IconName::GitMerge,
                None,
                move |v, _, cx| {
                    let id = id.clone();
                    v.store.update(cx, |s, cx| s.merge_thread(id, cx));
                },
            ));
            let id = t.id.clone();
            items.push(entry(
                "Create pull request",
                IconName::GitPullRequest,
                None,
                move |v, _, cx| {
                    let id = id.clone();
                    v.store.update(cx, |s, cx| s.create_pull_request(id, cx));
                },
            ));
        }
        let id = t.id.clone();
        items.push(entry(
            "Archive thread",
            IconName::Archive,
            None,
            move |v, _, cx| {
                let id = id.clone();
                v.store.update(cx, |s, cx| s.archive_thread(id, false, cx));
            },
        ));
        groups.push(("This thread".to_owned(), items));
    }

    let app = vec![
        entry(
            "New thread",
            IconName::SquarePen,
            Some(Box::new(NewThread)),
            |v, _, cx| v.new_thread(None, cx),
        ),
        entry(
            "New tab",
            IconName::Plus,
            Some(Box::new(NewTab)),
            |_, window, cx| {
                let tab = crate::windows::Placement::TabOf(window.window_handle());
                crate::windows::open(None, tab, cx)
            },
        ),
        entry(
            "New window",
            IconName::Plus,
            Some(Box::new(NewWindow)),
            |_, _, cx| crate::windows::open(None, crate::windows::Placement::Window, cx),
        ),
        entry(
            "Add project…",
            IconName::FolderPlus,
            Some(Box::new(AddProject)),
            |v, _, cx| v.add_project(cx),
        ),
        entry(
            "Show files",
            IconName::FolderTree,
            Some(Box::new(ShowFiles)),
            |v, w, cx| v.show_tab(Tab::Files, w, cx),
        ),
        entry(
            "Show changes",
            IconName::GitCompare,
            Some(Box::new(ShowChanges)),
            |v, w, cx| v.show_tab(Tab::Changes, w, cx),
        ),
        entry(
            "Show terminal",
            IconName::SquareTerminal,
            Some(Box::new(ShowTerminal)),
            |v, w, cx| v.show_tab(Tab::Terminal, w, cx),
        ),
        entry(
            "Show browser",
            IconName::Globe,
            Some(Box::new(ShowBrowser)),
            |v, w, cx| v.show_tab(Tab::Browser, w, cx),
        ),
        entry(
            "Toggle sidebar",
            IconName::PanelLeft,
            Some(Box::new(ToggleSidebar)),
            |v, _, cx| {
                v.sidebar_open = !v.sidebar_open;
                cx.notify();
            },
        ),
        entry(
            "Toggle panel",
            IconName::PanelRight,
            Some(Box::new(TogglePanel)),
            |v, _, cx| {
                v.panel_open = !v.panel_open;
                cx.notify();
            },
        ),
        entry(
            "Toggle light/dark",
            IconName::Moon,
            Some(Box::new(ToggleTheme)),
            |_, _, cx| crate::theme::toggle_mode(cx),
        ),
        entry(
            "Settings",
            IconName::Settings,
            Some(Box::new(OpenSettings)),
            |v, _, cx| v.open_settings(cx),
        ),
    ];
    groups.push(("Agentloft".to_owned(), app));
    groups
}
