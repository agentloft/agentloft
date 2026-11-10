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
use gpui_kit::{
    Action, App, AppContext as _, Context, Entity, ParentElement as _, Styled as _, Window, px,
};
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

/// Message search results for the palette's current query.
#[derive(Default)]
struct Found {
    seq: u64,
    query: String,
    hits: Vec<(loft_protocol::SearchHit, String)>,
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
    let found = cx.new(|_| Found::default());
    let focus = state.clone();
    let store = store.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let view = view.clone();
        // What's typed also searches every thread's messages: those hits
        // go after the threads whose titles match.
        let mut all: Vec<(String, Rc<Vec<Entry>>)> = groups
            .iter()
            .map(|(label, entries)| (label.clone(), entries.clone()))
            .collect();
        let messages = message_entries(&found, cx);
        if !messages.is_empty() {
            let at = all
                .iter()
                .position(|(l, _)| l == "Threads")
                .map_or(0, |i| i + 1);
            all.insert(at, ("Messages".to_owned(), Rc::new(messages)));
        }
        let all = Rc::new(all);
        let mut command = Command::new(&state)
            .placeholder("Search threads, messages, settings and commands…")
            .bordered(false)
            .max_h(px(460.))
            .footer(|_, _, cx| key_hints(cx));
        for (label, entries) in all.iter() {
            command = command.group(
                CommandGroup::new()
                    .label(label.clone())
                    .items(entries.iter().map(|e| e.item.clone())),
            );
        }
        let (found_q, store_q) = (found.clone(), store.clone());
        // Esc clears what's typed, then closes (the field would otherwise
        // keep the key to itself).
        let esc_state = state.clone();
        use gpui_kit::InteractiveElement as _;
        let esc =
            gpui_kit::div().capture_key_down(move |event: &gpui_kit::KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                cx.stop_propagation();
                if esc_state.read(cx).query(cx).is_empty() {
                    window.close_dialog(cx);
                } else {
                    esc_state.update(cx, |s, cx| s.set_query("", window, cx));
                }
            });
        dialog
            .w(px(640.))
            .p_0()
            .overflow_hidden()
            .margin_top(px(96.))
            .close_button(false)
            .child(
                esc.child(
                    command
                        .on_query(move |query, window, cx| {
                            search_messages(&found_q, &store_q, query, window, cx)
                        })
                        .on_confirm(move |ix: IndexPath, window, cx| {
                            let Some(run) = all
                                .get(ix.section)
                                .and_then(|(_, entries)| entries.get(ix.row))
                                .map(|e| e.run.clone())
                            else {
                                return;
                            };
                            window.close_dialog(cx);
                            view.update(cx, |v, cx| run(v, window, cx));
                        }),
                ),
            )
    });
    focus.update(cx, |s, cx| s.focus(window, cx));
}

/// "↑ ↓ Navigate · Enter Select · Esc Close" under the list.
fn key_hints(cx: &App) -> gpui_kit::AnyElement {
    use gpui_kit::component::Sizable as _;
    use gpui_kit::{IntoElement as _, div};
    let p = crate::theme::Palette::of(cx);
    let key = |child: gpui_kit::AnyElement| {
        div()
            .h(px(18.))
            .min_w(px(18.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .bg(p.muted)
            .border_1()
            .border_color(p.border)
            .text_color(p.foreground)
            .child(child)
    };
    let word = |w: &'static str| div().child(w).into_any_element();
    let icon = |i: IconName| {
        gpui_kit::component::Icon::new(i)
            .xsmall()
            .into_any_element()
    };
    let hint = |keys: Vec<gpui_kit::AnyElement>, what: &'static str| {
        gpui_kit::component::h_flex()
            .gap_1()
            .items_center()
            .children(keys.into_iter().map(key))
            .child(div().ml(px(2.)).child(what))
    };
    gpui_kit::component::h_flex()
        .w_full()
        .gap_4()
        .px_3()
        .py_2()
        .items_center()
        .border_t_1()
        .border_color(p.border)
        .text_xs()
        .text_color(p.muted_foreground)
        .child(hint(
            vec![icon(IconName::ArrowUp), icon(IconName::ArrowDown)],
            "Navigate",
        ))
        .child(hint(vec![word("Enter")], "Select"))
        .child(hint(vec![word("Esc")], "Close"))
        .into_any_element()
}

/// Ask the core which threads' messages mention `query` (after a pause).
fn search_messages(
    found: &Entity<Found>,
    store: &Entity<crate::store::AppStore>,
    query: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let query = query.trim().to_owned();
    let seq = found.update(cx, |f, _| {
        f.seq += 1;
        if query.chars().count() < 2 {
            f.hits.clear();
            f.query.clear();
        }
        f.seq
    });
    if query.chars().count() < 2 {
        window.refresh();
        return;
    }
    let Some(client) = store.read(cx).client() else {
        return;
    };
    let (found, store) = (found.clone(), store.clone());
    window
        .spawn(cx, async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(140))
                .await;
            if cx.update(|_, cx| found.read(cx).seq != seq).unwrap_or(true) {
                return;
            }
            let result = client
                .request(loft_protocol::Request::SearchThreads {
                    query: query.clone(),
                    limit: 30,
                })
                .await;
            let _ = cx.update(|window, cx| {
                if found.read(cx).seq != seq {
                    return;
                }
                let titles: std::collections::HashMap<_, _> = store
                    .read(cx)
                    .workspace
                    .threads
                    .iter()
                    .map(|t| (t.id.clone(), t.title.clone()))
                    .collect();
                let hits = match result {
                    Ok(loft_protocol::Response::SearchHits { hits }) => hits
                        .into_iter()
                        .filter_map(|h| Some((titles.get(&h.thread_id)?.clone(), h)))
                        // Those already show under Threads.
                        .filter(|(title, _)| !title.to_lowercase().contains(&query.to_lowercase()))
                        .map(|(title, h)| (h, title))
                        .collect(),
                    _ => Vec::new(),
                };
                found.update(cx, |f, _| {
                    f.hits = hits;
                    f.query = query;
                });
                window.refresh();
            });
        })
        .detach();
}

/// A thread in the palette: its agent's logo, title and project.
fn thread_row(
    title: &str,
    project: &str,
    harness: &HarnessId,
    fork: bool,
    branch: bool,
    cx: &App,
) -> gpui_kit::AnyElement {
    use gpui_kit::component::Sizable as _;
    use gpui_kit::{IntoElement as _, Styled as _, div, prelude::FluentBuilder as _};
    let p = crate::theme::Palette::of(cx);
    let mark = |icon: IconName| {
        gpui_kit::component::Icon::new(icon)
            .xsmall()
            .text_color(p.faint_foreground)
    };
    gpui_kit::component::h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .child(crate::brand::agent_logo(harness.as_str(), px(15.), &p))
        .child(div().min_w_0().truncate().child(title.to_owned()))
        .when(fork, |d| d.child(mark(IconName::GitFork)))
        .when(branch, |d| d.child(mark(IconName::GitBranch)))
        .child(
            div()
                .flex_none()
                .text_sm()
                .text_color(p.muted_foreground)
                .child(project.to_owned()),
        )
        .into_any_element()
}

/// The palette rows for message hits.
fn message_entries(found: &Entity<Found>, cx: &App) -> Vec<Entry> {
    let found = found.read(cx);
    found
        .hits
        .iter()
        .map(|(hit, title)| {
            let (hit2, title2) = (hit.clone(), title.clone());
            let (id, item) = (hit.thread_id.clone(), hit.item_id.clone());
            Entry {
                item: CommandItem::new()
                    .label(title.clone())
                    // It matched the query that found it.
                    .keywords([found.query.clone()])
                    .child(move |_, cx| message_row(&hit2, &title2, cx)),
                run: Rc::new(move |v, _, cx| {
                    let (id, item) = (id.clone(), item.clone());
                    v.store.update(cx, |s, cx| s.select_thread(id.clone(), cx));
                    let thread = v.store.update(cx, |s, cx| s.ensure_thread(&id, cx));
                    thread.update(cx, |t, cx| t.reveal(item, cx));
                }),
            }
        })
        .collect()
}

fn message_row(hit: &loft_protocol::SearchHit, title: &str, cx: &App) -> gpui_kit::AnyElement {
    use gpui_kit::{FontWeight, IntoElement as _, Styled as _, div};
    let p = crate::theme::Palette::of(cx);
    let text = hit.snippet.clone();
    let ok = hit.start <= hit.end
        && hit.end <= text.len()
        && text.is_char_boundary(hit.start)
        && text.is_char_boundary(hit.end);
    let styled = gpui_kit::StyledText::new(text).with_highlights(ok.then(|| {
        (
            hit.start..hit.end,
            gpui_kit::HighlightStyle {
                color: Some(p.foreground),
                font_weight: Some(FontWeight::SEMIBOLD),
                background_color: Some(p.accent.opacity(0.22)),
                ..Default::default()
            },
        )
    }));
    gpui_kit::component::h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .child(
            gpui_kit::component::Icon::new(if hit.from_user {
                IconName::User
            } else {
                IconName::MessageSquare
            })
            .text_color(p.muted_foreground),
        )
        .child(
            gpui_kit::component::v_flex()
                .flex_1()
                .min_w_0()
                .child(div().truncate().child(title.to_owned()))
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(p.muted_foreground)
                        .child(styled),
                ),
        )
        .into_any_element()
}

fn build(store: &Entity<crate::store::AppStore>, cx: &App) -> Vec<(String, Rc<Vec<Entry>>)> {
    let store_entity = store.clone();
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
        .filter(|t| t.is_started() || t.parent.is_some() || t.draft_text().is_some())
        .map(|t| {
            let project = store
                .workspace
                .projects
                .iter()
                .find(|p| p.id == t.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let mut keywords = vec![project.clone(), harness_name(&t.harness).to_owned()];
            keywords.extend(t.workspace.branch().map(str::to_owned));
            let id = t.id.clone();
            let (title, harness) = (t.title.clone(), t.harness.clone());
            let (fork, branch) = (t.parent.is_some(), t.workspace.branch().is_some());
            Entry {
                item: CommandItem::new()
                    .label(format!("{} — {project}", t.title))
                    .keywords(keywords)
                    .child(move |_, cx| thread_row(&title, &project, &harness, fork, branch, cx)),
                run: Rc::new(move |v, _, cx| {
                    let id = id.clone();
                    v.store.update(cx, |s, cx| s.select_thread(id, cx));
                }),
            }
        })
        .collect();
    if !threads.is_empty() {
        groups.push(("Threads".to_owned(), Rc::new(threads)));
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
        for (h, name) in store.available_harnesses() {
            let id = t.id.clone();
            items.push(entry(
                format!("Fork with {name}"),
                IconName::GitFork,
                None,
                move |v, _, cx| {
                    let id = id.clone();
                    v.store
                        .update(cx, |s, cx| s.fork_thread(id, HarnessId(h.into()), None, cx));
                },
            ));
        }
        let id = t.id.clone();
        items.push(entry(
            "Open in new pane",
            IconName::Columns2,
            None,
            move |v, _, cx| {
                let id = id.clone();
                v.store.update(cx, |s, cx| s.open_pane(id, cx));
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
        groups.push(("This thread".to_owned(), Rc::new(items)));
    }

    let app = vec![
        entry(
            "New thread",
            IconName::SquarePen,
            Some(Box::new(NewThread)),
            |v, _, cx| v.new_thread(None, cx),
        ),
        entry(
            "New pane",
            IconName::Columns2,
            Some(Box::new(NewTab)),
            |v, _, cx| v.new_pane(cx),
        ),
        entry(
            "New window",
            IconName::Plus,
            Some(Box::new(NewWindow)),
            |_, _, cx| {
                crate::windows::open(None, crate::windows::Placement::Window, cx);
            },
        ),
        entry(
            "Add project…",
            IconName::FolderPlus,
            Some(Box::new(AddProject)),
            |v, window, cx| {
                v.add_project(window, cx);
            },
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
    groups.push(("Agentloft".to_owned(), Rc::new(app)));

    // Projects: start a thread in one.
    let projects: Vec<Entry> = store
        .workspace
        .projects
        .iter()
        .map(|p| {
            let id = p.id.clone();
            let mut e = entry(
                format!("New thread in {}", p.name),
                IconName::FolderOpen,
                None,
                move |v, _, cx| v.new_thread(Some(id.clone()), cx),
            );
            e.item = e.item.keywords(["project".to_owned(), p.root_path.clone()]);
            e
        })
        .collect();
    if !projects.is_empty() {
        groups.push(("Projects".to_owned(), Rc::new(projects)));
    }

    // Settings: each opens its page.
    let mut settings: Vec<Entry> = crate::settings::FINDABLE
        .iter()
        .map(|f| {
            let (section, store) = (f.section, store_entity.clone());
            let mut e = entry(
                format!("{} › {}", f.section.label(), f.label),
                f.section.icon(),
                None,
                move |_, _, cx| crate::settings::open(store.clone(), Some(section), cx),
            );
            e.item = e.item.keywords(
                std::iter::once("settings".to_owned())
                    .chain(f.keywords.split(' ').map(str::to_owned)),
            );
            e
        })
        .collect();
    for &(id, name) in crate::settings::agents() {
        let store = store_entity.clone();
        let mut e = entry(
            format!("Agents › {name}"),
            IconName::Bot,
            None,
            move |_, _, cx| crate::settings::open_agent(store.clone(), id, cx),
        );
        e.item = e.item.keywords(
            [
                "settings",
                "agent",
                "model",
                "effort",
                "permissions",
                "binary",
                "environment",
            ]
            .map(str::to_owned),
        );
        settings.push(e);
    }
    groups.push(("Settings".to_owned(), Rc::new(settings)));

    // Themes: switch at once.
    let current = crate::theme::palette_id(cx);
    let paths = store.paths().clone();
    let themes: Vec<Entry> = crate::themes::all(&paths.home)
        .into_iter()
        .map(|t| {
            let (id, paths) = (t.id.clone(), paths.clone());
            let mut e = entry(
                format!("Theme: {}", t.name),
                IconName::Palette,
                None,
                move |_, _, cx| {
                    let value = loft_config::SettingValue::Str(id.clone());
                    if loft_config::write_setting(&paths, &["ui", "palette"], value).is_ok() {
                        crate::theme::reload_if_changed(cx);
                    }
                },
            );
            e.item = e
                .item
                .keywords(["theme", "colours", "colors", "appearance"].map(str::to_owned))
                .checked(current.as_deref() == Some(t.id.as_str()));
            e
        })
        .collect();
    groups.push(("Themes".to_owned(), Rc::new(themes)));
    groups
}
