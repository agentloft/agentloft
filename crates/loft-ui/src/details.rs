//! The thread's details card (title-bar button): where it runs, open in
//! an editor, its branch and changes, where it was forked from. On a wide
//! window it docks beside the conversation; on a narrow one it floats in a
//! small window (so it shows over the browser panel too).

use crate::panel::Tab;
use crate::store::AppStore;
use crate::theme::Palette;
use crate::workspace::WorkspaceView;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point,
    prelude::FluentBuilder as _, px, size,
};
use loft_protocol::{DiffScope, Request, Response, ThreadId, ThreadParent, ThreadWorkspace};
use std::cell::Cell;
use std::path::PathBuf;

const WIDTH: f32 = 340.;
const ROW: f32 = 34.;
const PAD: f32 = 6.;
const DIVIDER: f32 = 13.;
/// A section heading ("Subagents").
const SECTION: f32 = 24.;
/// Subagents listed (the most recent).
const MAX_AGENTS: usize = 8;

/// The thread's subagents, most recent last.
fn subagents(store: &AppStore, thread_id: &ThreadId, cx: &App) -> Vec<loft_protocol::Item> {
    let Some(thread) = store.threads.get(thread_id) else {
        return Vec::new();
    };
    let Some(model) = thread.read(cx).model.as_ref() else {
        return Vec::new();
    };
    let all: Vec<_> = model
        .items
        .iter()
        .filter(|i| {
            i.parent_id.is_none()
                && matches!(&i.body, loft_protocol::ItemBody::ToolCall { tool_kind, .. }
                    if *tool_kind == loft_protocol::ToolKind::Subagent)
        })
        .cloned()
        .collect();
    let skip = all.len().saturating_sub(MAX_AGENTS);
    all.into_iter().skip(skip).collect()
}

/// The computer row has two lines (name, folder).
const TALL_ROW: f32 = 48.;

/// Editors we can open a folder in: (app name, label).
const EDITORS: &[(&str, &str)] = &[
    ("Visual Studio Code", "VS Code"),
    ("Cursor", "Cursor"),
    ("Zed", "Zed"),
    ("Windsurf", "Windsurf"),
    ("Sublime Text", "Sublime Text"),
    ("Xcode", "Xcode"),
    ("Terminal", "Terminal"),
];

thread_local! {
    /// The editor picked last (this session).
    static EDITOR: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Where editor `e` is installed, if it is.
fn app_path(e: usize) -> Option<PathBuf> {
    let name = format!("{}.app", EDITORS[e].0);
    [
        Some(PathBuf::from("/Applications")),
        Some(PathBuf::from("/System/Applications/Utilities")),
        std::env::home_dir().map(|h| h.join("Applications")),
    ]
    .into_iter()
    .flatten()
    .map(|dir| dir.join(&name))
    .find(|path| path.exists())
}

fn installed_editors() -> Vec<usize> {
    (0..EDITORS.len())
        .filter(|&e| app_path(e).is_some())
        .collect()
}

/// Editor `e`'s own app icon.
fn editor_icon(e: usize) -> Option<std::sync::Arc<gpui_kit::Image>> {
    crate::app_icons::icon(&app_path(e)?)
}

fn open_in(editor: usize, folder: &str) {
    let _ = std::process::Command::new("open")
        .args(["-a", EDITORS[editor].0, folder])
        .spawn();
}

struct Info {
    cwd: String,
    workspace: ThreadWorkspace,
    parent: Option<(ThreadParent, String)>,
}

fn thread_info(store: &AppStore, id: &ThreadId) -> Option<Info> {
    let thread = store.workspace.threads.iter().find(|t| t.id == *id)?;
    let parent = thread.parent.clone().map(|p| {
        let title = store
            .workspace
            .threads
            .iter()
            .find(|t| t.id == p.thread_id)
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "another thread".into());
        (p, title)
    });
    Some(Info {
        cwd: thread.cwd.clone(),
        workspace: thread.workspace.clone(),
        parent,
    })
}

/// Open the card under `anchor` (bounds in `window`), right edges aligned.
pub fn open(
    window: &Window,
    anchor: Bounds<Pixels>,
    workspace: WeakEntity<WorkspaceView>,
    store: gpui_kit::Entity<AppStore>,
    thread_id: ThreadId,
    cx: &mut App,
) {
    let Some(info) = thread_info(store.read(cx), &thread_id) else {
        return;
    };
    let agents = subagents(store.read(cx), &thread_id, cx)
        .len()
        .min(MAX_AGENTS) as f32;
    let rows = 3. + if info.parent.is_some() { 1. } else { 0. } + agents;
    let dividers = 1. + if info.parent.is_some() { 1. } else { 0. } + agents.min(1.);
    let heading = if agents > 0. { SECTION } else { 0. };
    let height = PAD * 2. + TALL_ROW + rows * ROW + dividers * DIVIDER + heading + 4.;
    let win = window.bounds();
    let x = f32::from(win.origin.x + anchor.origin.x + anchor.size.width) - WIDTH;
    let y = f32::from(win.origin.y + anchor.origin.y + anchor.size.height) + 4.;
    let main = window.window_handle();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(x.max(0.)), px(y)),
            size(px(WIDTH), px(height)),
        ))),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: window.display(cx).map(|d| d.id()),
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        cx.new(|cx| DetailsCard::new(store, workspace, main, thread_id, info, true, window, cx))
    });
}

/// The card for docking beside the conversation in `window`.
pub fn docked(
    window: &mut Window,
    workspace: WeakEntity<WorkspaceView>,
    store: gpui_kit::Entity<AppStore>,
    thread_id: ThreadId,
    cx: &mut App,
) -> Option<gpui_kit::Entity<DetailsCard>> {
    let info = thread_info(store.read(cx), &thread_id)?;
    let main = window.window_handle();
    Some(cx.new(|cx| DetailsCard::new(store, workspace, main, thread_id, info, false, window, cx)))
}

pub struct DetailsCard {
    store: gpui_kit::Entity<AppStore>,
    workspace: WeakEntity<WorkspaceView>,
    main: AnyWindowHandle,
    thread_id: ThreadId,
    info: Info,
    /// In its own pop-up window (else docked in the main window).
    floating: bool,
    /// The last finished turn whose changes are counted.
    counted_turn: Option<loft_protocol::TurnId>,
    focus: FocusHandle,
    computer: Option<String>,
    /// The checkout's branch (for a thread working in the project itself).
    local_branch: Option<String>,
    /// Files, lines added, lines removed in this thread.
    changes: Option<(usize, u32, u32)>,
    editors: Vec<usize>,
    /// Where the editor ⌄ is (its menu opens under it).
    editor_anchor: std::rc::Rc<Cell<Option<Bounds<Pixels>>>>,
    copied: bool,
}

impl DetailsCard {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: gpui_kit::Entity<AppStore>,
        workspace: WeakEntity<WorkspaceView>,
        main: AnyWindowHandle,
        thread_id: ThreadId,
        info: Info,
        floating: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        // Subagents come and go: keep their list current.
        if let Some(thread) = store.read(cx).threads.get(&thread_id).cloned() {
            cx.observe(&thread, |_, _, cx| cx.notify()).detach();
        }
        if floating {
            window.focus(&focus, cx);
            cx.observe_window_activation(window, |_, window, _| {
                if !window.is_window_active() {
                    window.remove_window();
                }
            })
            .detach();
        } else if let Some(thread) = store.read(cx).threads.get(&thread_id).cloned() {
            // Docked, it stays open: recount changes after each turn.
            cx.observe(&thread, |this, thread, cx| {
                let done = thread
                    .read(cx)
                    .model
                    .as_ref()
                    .and_then(|m| m.last_turn())
                    .filter(|t| t.status.is_terminal())
                    .map(|t| t.id.clone());
                if done.is_some() && done != this.counted_turn {
                    this.counted_turn = done;
                    this.count_changes(cx);
                }
            })
            .detach();
        }
        let cwd = info.cwd.clone();
        let local = matches!(info.workspace, ThreadWorkspace::Local);
        cx.spawn(async move |this, cx| {
            let (computer, branch) = cx
                .background_executor()
                .spawn(async move {
                    let run = |cmd: &str, args: &[&str]| {
                        std::process::Command::new(cmd)
                            .args(args)
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                            .filter(|s| !s.is_empty())
                    };
                    let computer = run("scutil", &["--get", "ComputerName"]);
                    let branch = local
                        .then(|| run("git", &["-C", &cwd, "rev-parse", "--abbrev-ref", "HEAD"]))
                        .flatten();
                    (computer, branch)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.computer = computer;
                this.local_branch = branch;
                cx.notify();
            });
        })
        .detach();
        let counted_turn = store
            .read(cx)
            .threads
            .get(&thread_id)
            .and_then(|t| t.read(cx).model.as_ref()?.last_turn().map(|t| t.id.clone()));
        let mut this = Self {
            store,
            workspace,
            main,
            thread_id,
            info,
            floating,
            counted_turn,
            focus,
            computer: None,
            local_branch: None,
            changes: None,
            editors: installed_editors(),
            editor_anchor: Default::default(),
            copied: false,
        };
        this.count_changes(cx);
        this
    }

    fn count_changes(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let body = Request::GetDiff {
            thread_id: self.thread_id.clone(),
            scope: DiffScope::Thread,
        };
        cx.spawn(async move |this, cx| {
            if let Ok(Response::Diff { diff }) = client.request(body).await {
                let added = diff.files.iter().map(|f| f.added).sum();
                let removed = diff.files.iter().map(|f| f.removed).sum();
                let _ = this.update(cx, |this, cx| {
                    this.changes = Some((diff.files.len(), added, removed));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// Close the card if it is the floating one.
    fn dismiss(floating: bool, window: &mut Window) {
        if floating {
            window.remove_window();
        }
    }

    /// Run `f` on the workspace in the main window, then close the card.
    fn in_main(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut WorkspaceView, &mut Window, &mut Context<WorkspaceView>) + 'static,
    ) {
        let workspace = self.workspace.clone();
        if !self.floating {
            // Docked: we are in the main window already.
            _ = workspace.update(cx, |w, cx| f(w, window, cx));
            return;
        }
        window.remove_window();
        let _ = self.main.update(cx, move |_, window, cx| {
            _ = workspace.update(cx, |w, cx| f(w, window, cx));
        });
    }

    fn editor(&self) -> Option<usize> {
        EDITOR
            .get()
            .filter(|e| self.editors.contains(e))
            .or_else(|| self.editors.first().copied())
    }
}

impl Focusable for DetailsCard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn row(id: &str, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(SharedString::from(id.to_owned()))
        .h(px(ROW))
        .px_2()
        .gap_2p5()
        .items_center()
        .rounded(px(7.))
        .text_sm()
        .text_color(p.foreground)
}

fn divider(p: &Palette) -> AnyElement {
    div()
        .h(px(DIVIDER))
        .flex()
        .items_center()
        .child(div().w_full().h_px().bg(p.border))
        .into_any_element()
}

fn icon(name: IconName, p: &Palette) -> AnyElement {
    Icon::new(name)
        .small()
        .text_color(p.muted_foreground)
        .into_any_element()
}

impl Render for DetailsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let cwd = self.info.cwd.clone();
        let floating = self.floating;
        let mut list = v_flex().p(px(PAD));

        // Where it runs.
        let folder = cwd.clone();
        list = list.child(
            row("details-computer", &p)
                .h(px(TALL_ROW))
                .child(icon(IconName::Laptop, &p))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .child(self.computer.clone().unwrap_or_else(|| "This Mac".into())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child(crate::workspace::tilde_path_str(&cwd)),
                        ),
                )
                .child(
                    div()
                        .id("details-reveal")
                        .p(px(4.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_color(p.muted_foreground)
                        .hover(|d| d.bg(p.hover).text_color(p.foreground))
                        .child(Icon::new(IconName::FolderOpen).small())
                        .on_click(move |_, window, cx| {
                            cx.open_with_system(std::path::Path::new(&folder));
                            Self::dismiss(floating, window);
                        }),
                ),
        );

        // Open in an editor.
        let editor = self.editor();
        list = list.child(match editor {
            Some(ed) => {
                let folder = cwd.clone();
                row("details-editor", &p)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .child(match editor_icon(ed) {
                        Some(picture) => {
                            use gpui_kit::StyledImage as _;
                            gpui_kit::img(picture)
                                .size(px(16.))
                                .object_fit(gpui_kit::ObjectFit::Contain)
                                .into_any_element()
                        }
                        None => icon(IconName::Code, &p),
                    })
                    .child(div().flex_1().child(format!("Open in {}", EDITORS[ed].1)))
                    .on_click(move |_, window, _| {
                        open_in(ed, &folder);
                        Self::dismiss(floating, window);
                    })
                    .when(self.editors.len() > 1, |d| {
                        let (anchor, editors, folder) = (
                            self.editor_anchor.clone(),
                            self.editors.clone(),
                            cwd.clone(),
                        );
                        let measure = anchor.clone();
                        d.child(
                            div()
                                .relative()
                                .child(
                                    div()
                                        .id("details-editor-pick")
                                        .p(px(4.))
                                        .rounded(px(5.))
                                        .text_color(p.muted_foreground)
                                        .hover(|d| d.bg(p.selected).text_color(p.foreground))
                                        .child(Icon::new(IconName::ChevronDown).xsmall())
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            let Some(at) = anchor.get() else {
                                                return;
                                            };
                                            let entries = editors
                                                .iter()
                                                .map(|&e| {
                                                    let folder = folder.clone();
                                                    loft_browser::menu::Entry::item(
                                                        format!("Open in {}", EDITORS[e].1),
                                                        IconName::Code,
                                                        move |_| {
                                                            EDITOR.set(Some(e));
                                                            open_in(e, &folder);
                                                        },
                                                    )
                                                    .checked(Some(e) == editor)
                                                    .image(editor_icon(e))
                                                })
                                                .collect();
                                            loft_browser::menu::open(window, at, entries, cx);
                                        }),
                                )
                                .child(
                                    gpui_kit::canvas(
                                        move |bounds, _, _| measure.set(Some(bounds)),
                                        |_, _, _, _| {},
                                    )
                                    .absolute()
                                    .size_full(),
                                ),
                        )
                    })
            }
            None => row("details-editor", &p)
                .child(icon(IconName::Code, &p))
                .child(
                    div()
                        .text_color(p.muted_foreground)
                        .child("No editor found in Applications"),
                ),
        });
        list = list.child(divider(&p));

        // Branch.
        let (branch, base) = match &self.info.workspace {
            ThreadWorkspace::Worktree { branch, base, .. } => {
                (Some(branch.clone()), Some(base.clone()))
            }
            ThreadWorkspace::NewWorktree { .. } => {
                (Some("New worktree (on first message)".into()), None)
            }
            ThreadWorkspace::Local => (self.local_branch.clone(), None),
        };
        let copy = branch.clone();
        list = list.child(
            row("details-branch", &p)
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child(icon(IconName::GitBranch, &p))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(branch.unwrap_or_else(|| "No git branch".into())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(if self.copied {
                            "Copied".to_owned()
                        } else {
                            base.map(|b| format!("from {b}")).unwrap_or_default()
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(name) = &copy {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(name.clone()));
                        this.copied = true;
                        cx.notify();
                    }
                })),
        );

        // Changes in this thread.
        let changes = match self.changes {
            None => div()
                .text_color(p.faint_foreground)
                .child("…")
                .into_any_element(),
            Some((0, _, _)) => div()
                .text_color(p.faint_foreground)
                .child("None yet")
                .into_any_element(),
            Some((files, added, removed)) => h_flex()
                .gap_2()
                .font_family(p.mono.clone())
                .text_xs()
                .child(
                    div()
                        .text_color(p.faint_foreground)
                        .child(format!("{files} file{}", if files == 1 { "" } else { "s" })),
                )
                .child(div().text_color(p.success).child(format!("+{added}")))
                .child(div().text_color(p.danger).child(format!("−{removed}")))
                .into_any_element(),
        };
        list = list.child(
            row("details-changes", &p)
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child(icon(IconName::FileDiff, &p))
                .child(div().flex_1().child("Changes"))
                .child(changes)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.in_main(window, cx, |w, window, cx| {
                        w.open_tab(Tab::Changes, window, cx)
                    });
                })),
        );

        // Subagents: open one, or stop it while it runs.
        let (agents, harness) = {
            let store = self.store.read(cx);
            let agents = subagents(store, &self.thread_id, cx);
            let harness = store
                .threads
                .get(&self.thread_id)
                .and_then(|t| t.read(cx).model.as_ref().map(|m| m.thread.harness.clone()));
            (agents, harness)
        };
        if !agents.is_empty() {
            let model_state = self.store.read(cx).threads.get(&self.thread_id).cloned();
            list = list.child(divider(&p)).child(
                div()
                    .h(px(SECTION))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child("Subagents"),
            );
            let can_stop = harness.as_ref().is_some_and(|h| h.as_str() == "claude");
            for agent in agents {
                let Some(thread) = model_state.as_ref() else {
                    break;
                };
                let status = {
                    let state = thread.read(cx);
                    let Some(model) = state.model.as_ref() else {
                        break;
                    };
                    crate::items::subagent_status(model, &agent)
                };
                let title = match &agent.body {
                    loft_protocol::ItemBody::ToolCall { title, .. } => title.clone(),
                    _ => String::new(),
                };
                let dot = if status.running {
                    p.accent
                } else if status.failed {
                    p.danger
                } else {
                    p.success
                };
                let (open_id, stop_id) = (agent.id.clone(), agent.id.clone());
                list = list.child(
                    row(&format!("details-agent-{}", agent.id), &p)
                        .group("details-agent")
                        .cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .child(crate::items::agent_avatar(
                            &format!("details-dot-{}", agent.id),
                            harness.as_ref().map(|h| h.as_str()).unwrap_or("claude"),
                            22.,
                            dot,
                            status.running,
                            &p,
                        ))
                        .child(div().flex_1().min_w_0().truncate().child(title))
                        .children(
                            status
                                .elapsed
                                .map(|e| div().text_xs().text_color(p.faint_foreground).child(e)),
                        )
                        .when(status.running && can_stop, |d| {
                            d.child(
                                div()
                                    .id(SharedString::from(format!(
                                        "details-agent-stop-{}",
                                        stop_id
                                    )))
                                    .p(px(3.))
                                    .rounded(px(5.))
                                    .text_color(p.muted_foreground)
                                    .hover(|d| d.bg(p.danger_soft).text_color(p.danger))
                                    .child(Icon::new(IconName::CircleStop).small())
                                    .tooltip(|window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(
                                            "Stop this subagent",
                                        )
                                        .build(window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let (t, i) = (this.thread_id.clone(), stop_id.clone());
                                        this.store.update(cx, |s, cx| s.stop_subagent(t, i, cx));
                                    })),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let (store, t, i) =
                                (this.store.clone(), this.thread_id.clone(), open_id.clone());
                            this.in_main(window, cx, move |_, _, cx| {
                                store.update(cx, |s, cx| s.open_agent(t, i, cx));
                            });
                        })),
                );
            }
        }

        // Lineage.
        if let Some((parent, title)) = self.info.parent.clone() {
            list = list.child(divider(&p)).child(
                row("details-parent", &p)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .child(icon(IconName::GitFork, &p))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(format!("Forked from {title}")),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .xsmall()
                            .text_color(p.faint_foreground),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let (store, id) = (this.store.clone(), parent.thread_id.clone());
                        this.in_main(window, cx, move |_, _, cx| {
                            store.update(cx, |s, cx| s.select_thread(id, cx));
                        });
                    })),
            );
        }

        let card = list
            .bg(p.elevated)
            .border_1()
            .border_color(p.border)
            .rounded(px(12.))
            .font_weight(FontWeight::NORMAL);
        if !floating {
            return card.w_full().into_any_element();
        }
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(|event, window, _| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
            .child(card.size_full().shadow_lg())
            .into_any_element()
    }
}
