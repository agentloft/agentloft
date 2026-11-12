//! The thread's details card (title-bar button): where it runs, open in
//! an editor, its branch and changes, its subagents and the threads it
//! started, where it came from. On a wide window it docks beside the
//! conversation; on a narrow one it floats in a small window (so it shows
//! over the browser panel too), or over the page where there is one window
//! only (a browser tab).

use crate::panel::Tab;
use crate::press::ClaimsPress as _;
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
use loft_protocol::{
    DiffScope, ForkKind, ProjectId, Request, Response, ThreadId, ThreadParent, ThreadStatus,
    ThreadWorkspace,
};
use std::cell::Cell;
use std::path::PathBuf;

const WIDTH: f32 = 340.;
const ROW: f32 = 34.;
const PAD: f32 = 6.;
const DIVIDER: f32 = 13.;
/// A section heading ("Subagents").
const SECTION: f32 = 24.;
/// Room for the lists (subagents, started threads): a heading and five and
/// a half rows, the half saying there is more. Longer lists scroll.
const LISTS_MAX: f32 = SECTION + ROW * 5.5;

/// The thread's subagents, oldest first.
fn subagents(store: &AppStore, thread_id: &ThreadId, cx: &App) -> Vec<loft_protocol::Item> {
    let Some(thread) = store.threads.get(thread_id) else {
        return Vec::new();
    };
    let Some(model) = thread.read(cx).model.as_ref() else {
        return Vec::new();
    };
    model
        .items
        .iter()
        .filter(|i| {
            i.parent_id.is_none()
                && matches!(&i.body, loft_protocol::ItemBody::ToolCall { tool_kind, .. }
                    if *tool_kind == loft_protocol::ToolKind::Subagent)
        })
        .cloned()
        .collect()
}

/// The threads this one's agent started, oldest first.
fn started_threads(store: &AppStore, thread_id: &ThreadId) -> Vec<loft_protocol::Thread> {
    let mut all: Vec<_> = store
        .workspace
        .threads
        .iter()
        .filter(|t| {
            t.status != ThreadStatus::Archived
                && t.parent
                    .as_ref()
                    .is_some_and(|p| p.thread_id == *thread_id && p.kind == ForkKind::Spawned)
        })
        .map(loft_protocol::Thread::without_commands)
        .collect();
    all.sort_by_key(|t| t.created_at_ms);
    all
}

/// The thread's pull request, as Agentloft last heard about it.
fn pull_request(store: &AppStore, thread_id: &ThreadId) -> Option<loft_protocol::PullRequest> {
    let thread = store
        .workspace
        .threads
        .iter()
        .find(|t| t.id == *thread_id)?;
    thread.pull_request.clone()
}

/// Where a card is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Beside the conversation.
    Docked,
    /// In a small window of its own.
    Window,
    /// Over the main window's content.
    Overlay,
}

/// The card open over the window's content.
struct Overlay {
    view: gpui_kit::Entity<DetailsCard>,
    /// Its top-left corner.
    at: gpui_kit::Point<Pixels>,
    focused: bool,
}

impl gpui_kit::Global for Overlay {}

thread_local! {
    /// When the overlay last closed: the click that closed it may be on
    /// the button that would open it again.
    static CLOSED_AT: Cell<Option<web_time::Instant>> = const { Cell::new(None) };
}

fn close_overlay(cx: &mut App) {
    if cx.try_global::<Overlay>().is_some() {
        CLOSED_AT.set(Some(web_time::Instant::now()));
        cx.remove_global::<Overlay>();
        cx.refresh_windows();
    }
}

/// Whether the card is open over the window's content.
pub fn is_over(cx: &App) -> bool {
    cx.try_global::<Overlay>().is_some()
}

/// The card open over the window's content, for the window's root view to
/// draw above everything else. `None` when it isn't open (always, where it
/// gets a window of its own).
pub fn overlay(window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    let overlay = cx.try_global::<Overlay>()?;
    let (view, at, focused) = (overlay.view.clone(), overlay.at, overlay.focused);
    // It is about the thread in view, and goes with it.
    let card = view.read(cx);
    if card.store.read(cx).selected.as_ref() != Some(&card.thread_id) {
        cx.remove_global::<Overlay>();
        return None;
    }
    if !focused {
        window.focus(&view.focus_handle(cx), cx);
        cx.global_mut::<Overlay>().focused = true;
    }
    Some(
        gpui_kit::deferred(
            gpui_kit::anchored().position(at).child(
                div()
                    .occlude()
                    .w(px(WIDTH))
                    .on_mouse_down_out(|_, _, cx| close_overlay(cx))
                    .child(view),
            ),
        )
        .with_priority(9)
        .into_any_element(),
    )
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
    project: ProjectId,
    cwd: String,
    workspace: ThreadWorkspace,
    parent: Option<(ThreadParent, String)>,
}

fn thread_info(store: &AppStore, id: &ThreadId) -> Option<Info> {
    let thread = store.workspace.threads.iter().find(|t| t.id == *id)?;
    // A parent deleted since has nothing to open: no row for it.
    let parent = thread.parent.clone().and_then(|p| {
        let title = store
            .workspace
            .threads
            .iter()
            .find(|t| t.id == p.thread_id)
            .map(|t| t.title.clone())?;
        Some((p, title))
    });
    // A worktree that was cleaned up: until the next message makes a new
    // one, the thread's folder is the project's.
    let cwd = match &thread.workspace {
        ThreadWorkspace::Released { .. } => store
            .workspace
            .projects
            .iter()
            .find(|p| p.id == thread.project_id)
            .map(|p| p.root_path.clone())
            .unwrap_or_else(|| thread.cwd.clone()),
        _ => thread.cwd.clone(),
    };
    Some(Info {
        project: thread.project_id.clone(),
        cwd,
        workspace: thread.workspace.clone(),
        parent,
    })
}

/// Open the card under `anchor` (bounds in `window`), right edges aligned.
pub fn open(
    window: &mut Window,
    anchor: Bounds<Pixels>,
    workspace: WeakEntity<WorkspaceView>,
    store: gpui_kit::Entity<AppStore>,
    thread_id: ThreadId,
    cx: &mut App,
) {
    let Some(info) = thread_info(store.read(cx), &thread_id) else {
        return;
    };
    if crate::host::IS_WEB {
        // One window only: over its content, and as tall as it turns out.
        if is_over(cx) {
            close_overlay(cx);
            return;
        }
        if CLOSED_AT
            .get()
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(250))
        {
            return;
        }
        let room = f32::from(window.viewport_size().width);
        let x = (f32::from(anchor.origin.x + anchor.size.width) - WIDTH)
            .min(room - WIDTH - 4.)
            .max(4.);
        let y = f32::from(anchor.origin.y + anchor.size.height) + 4.;
        let main = window.window_handle();
        let view = cx.new(|cx| {
            let place = Place::Overlay;
            DetailsCard::new(store, workspace, main, thread_id, info, place, window, cx)
        });
        cx.set_global(Overlay {
            view,
            at: point(px(x), px(y)),
            focused: false,
        });
        cx.refresh_windows();
        return;
    }
    let agents = subagents(store.read(cx), &thread_id, cx).len() as f32;
    let threads = started_threads(store.read(cx), &thread_id).len() as f32;
    let parent = if info.parent.is_some() { 1. } else { 0. };
    // Its pull request has a row (two lines) when it has one.
    let pr = if pull_request(store.read(cx), &thread_id).is_some() {
        TALL_ROW
    } else {
        0.
    };
    // What the thread is (a line under it when lists follow), then the lists.
    let sections = agents.min(1.) + threads.min(1.);
    let top = TALL_ROW + pr + (3. + parent) * ROW + (1. + parent + sections.min(1.)) * DIVIDER;
    let lists = (agents + threads) * ROW + sections * SECTION + (sections - 1.).max(0.) * DIVIDER;
    let height = PAD * 2. + top + lists.min(LISTS_MAX) + 4.;
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
        cx.new(|cx| {
            let place = Place::Window;
            DetailsCard::new(store, workspace, main, thread_id, info, place, window, cx)
        })
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
    Some(cx.new(|cx| {
        let place = Place::Docked;
        DetailsCard::new(store, workspace, main, thread_id, info, place, window, cx)
    }))
}

pub struct DetailsCard {
    store: gpui_kit::Entity<AppStore>,
    workspace: WeakEntity<WorkspaceView>,
    main: AnyWindowHandle,
    thread_id: ThreadId,
    info: Info,
    place: Place,
    /// The last finished turn whose changes are counted.
    counted_turn: Option<loft_protocol::TurnId>,
    focus: FocusHandle,
    computer: Option<String>,
    /// The checkout's branch (for a thread working in the project itself).
    local_branch: Option<String>,
    /// Files, lines added, lines removed in this thread.
    changes: Option<(usize, u32, u32)>,
    /// The remote branch they are counted against (`origin/main`).
    changes_base: Option<String>,
    editors: Vec<usize>,
    /// Where the editor ⌄ is (its menu opens under it).
    editor_anchor: std::rc::Rc<Cell<Option<Bounds<Pixels>>>>,
    copied: bool,
    /// Whether the card has (room for) the pull request's row: one found
    /// while it floats in a window of its own makes that window taller.
    pr_row: bool,
}

impl DetailsCard {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: gpui_kit::Entity<AppStore>,
        workspace: WeakEntity<WorkspaceView>,
        main: AnyWindowHandle,
        thread_id: ThreadId,
        info: Info,
        place: Place,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        // Subagents come and go: keep their list current.
        if let Some(thread) = store.read(cx).threads.get(&thread_id).cloned() {
            cx.observe(&thread, |_, _, cx| cx.notify()).detach();
        }
        // So do the threads it starts, and what each is doing.
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        let local = matches!(info.workspace, ThreadWorkspace::Local);
        if crate::host::IS_WEB && local {
            // No git to ask in a browser: Agentloft knows the branch.
            let project = info.project.clone();
            store.update(cx, |store, cx| store.fetch_branches(project, cx));
        }
        if place == Place::Window {
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
        // The user is looking: have Agentloft ask GitHub how the pull
        // request stands now, or whether the branch has one by now.
        let pr_row = pull_request(store.read(cx), &thread_id).is_some();
        if pr_row || matches!(info.workspace, ThreadWorkspace::Worktree { .. }) {
            let id = thread_id.clone();
            store.update(cx, |store, cx| store.refresh_pull_request(id, cx));
        }
        let mut this = Self {
            store,
            workspace,
            main,
            thread_id,
            info,
            place,
            counted_turn,
            focus,
            computer: None,
            local_branch: None,
            changes: None,
            changes_base: None,
            editors: installed_editors(),
            editor_anchor: Default::default(),
            copied: false,
            pr_row,
        };
        this.count_changes(cx);
        this
    }

    fn count_changes(&mut self, cx: &mut Context<Self>) {
        // No worktree to compare: its work is in the base.
        if matches!(self.info.workspace, ThreadWorkspace::Released { .. }) {
            self.changes = Some((0, 0, 0));
            return;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let body = Request::GetDiff {
            thread_id: self.thread_id.clone(),
            scope: DiffScope::Origin,
        };
        cx.spawn(async move |this, cx| {
            if let Ok(Response::Diff { diff }) = client.request(body).await {
                let added = diff.files.iter().map(|f| f.added).sum();
                let removed = diff.files.iter().map(|f| f.removed).sum();
                let _ = this.update(cx, |this, cx| {
                    this.changes = Some((diff.files.len(), added, removed));
                    this.changes_base = diff.base;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// Close the card if it floats.
    fn dismiss(place: Place, window: &mut Window, cx: &mut App) {
        match place {
            Place::Docked => {}
            Place::Window => window.remove_window(),
            Place::Overlay => close_overlay(cx),
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
        if self.place != Place::Window {
            // We are in the main window already.
            Self::dismiss(self.place, window, cx);
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

/// A row of the card. Rows that do something light up under the pointer
/// with `p.selected`: `p.hover` is a shade for the window's background and
/// barely differs from the card's own (`p.elevated`).
fn row(id: &str, p: &Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(SharedString::from(id.to_owned()))
        // Rows keep their height: a card too tall for its place scrolls.
        .flex_none()
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
        .flex_none()
        .h(px(DIVIDER))
        .flex()
        .items_center()
        .child(div().w_full().h_px().bg(p.border))
        .into_any_element()
}

/// A list's heading ("Subagents").
fn section(label: &'static str, p: &Palette) -> AnyElement {
    div()
        .flex_none()
        .h(px(SECTION))
        .px_2()
        .flex()
        .items_center()
        .text_xs()
        .text_color(p.faint_foreground)
        .child(label)
        .into_any_element()
}

fn icon(name: IconName, p: &Palette) -> AnyElement {
    Icon::new(name)
        .small()
        .text_color(p.muted_foreground)
        .into_any_element()
}

impl Render for DetailsCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        // The thread may have changed under the card (its worktree cleaned
        // up, or made again).
        if let Some(info) = thread_info(self.store.read(cx), &self.thread_id) {
            let renewed = info.workspace != self.info.workspace;
            self.info = info;
            if renewed {
                self.changes = None;
                self.changes_base = None;
                self.count_changes(cx);
            }
        }
        let cwd = self.info.cwd.clone();
        let place = self.place;
        // What the thread is, then what it started: only the lists scroll.
        let mut top = v_flex().flex_none();

        // Where it runs.
        let folder = cwd.clone();
        top =
            top.child(
                row("details-computer", &p)
                    .h(px(TALL_ROW))
                    .child(icon(IconName::Laptop, &p))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(self.computer.clone().unwrap_or_else(
                                || {
                                    // In a browser the folder is on the
                                    // computer Agentloft runs on, not this one.
                                    if crate::host::IS_WEB {
                                        "Agentloft's computer".into()
                                    } else {
                                        "This Mac".into()
                                    }
                                },
                            )))
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(crate::workspace::tilde_path_str(&cwd)),
                            ),
                    )
                    // Showing the folder in Finder is this computer's to do.
                    .when(!crate::host::IS_WEB, |d| {
                        d.child(
                            div()
                                .id("details-reveal")
                                .p(px(4.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.selected).text_color(p.foreground))
                                .child(Icon::new(IconName::FolderOpen).small())
                                .on_click(move |_, window, cx| {
                                    cx.open_with_system(std::path::Path::new(&folder));
                                    Self::dismiss(place, window, cx);
                                }),
                        )
                    }),
            );

        // Open in an editor: this computer's to do, so not from a browser.
        let editor = self.editor();
        let editor_row = match editor {
            Some(ed) => {
                let folder = cwd.clone();
                row("details-editor", &p)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.selected))
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
                    .on_click(move |_, window, cx| {
                        open_in(ed, &folder);
                        Self::dismiss(place, window, cx);
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
                                        .hover(|d| d.bg(p.border).text_color(p.foreground))
                                        .child(Icon::new(IconName::ChevronDown).xsmall())
                                        .claims_press()
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
        };
        if !crate::host::IS_WEB {
            top = top.child(editor_row);
        }
        top = top.child(divider(&p));

        // Branch.
        let (branch, base) = match &self.info.workspace {
            ThreadWorkspace::Worktree { branch, base, .. } => {
                (Some(branch.clone()), Some(base.clone()))
            }
            ThreadWorkspace::NewWorktree { .. } => {
                (Some("New worktree (on first message)".into()), None)
            }
            ThreadWorkspace::Released { branch, base, .. } => {
                (Some(branch.clone()), Some(base.clone()))
            }
            ThreadWorkspace::Local => (
                self.local_branch.clone().or_else(|| {
                    let branches = self.store.read(cx).branches.get(&self.info.project)?;
                    branches.current.clone()
                }),
                None,
            ),
        };
        let copy = branch.clone();
        // Cleaned up: `Some(true)` when its branch was deleted and can be
        // brought back.
        let released = match &self.info.workspace {
            ThreadWorkspace::Released { tip, .. } => Some(tip.is_some()),
            _ => None,
        };
        let restore = released.filter(|deleted| *deleted).map(|_| {
            let (store, id) = (self.store.clone(), self.thread_id.clone());
            div()
                .id("details-restore-branch")
                .flex_none()
                .px_1p5()
                .rounded(px(5.))
                .text_xs()
                .text_color(p.accent)
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .claims_press()
                .child("Restore")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let id = id.clone();
                    store.update(cx, |s, cx| s.restore_branch(id, cx));
                })
        });
        top = top.child(
            row("details-branch", &p)
                .cursor_pointer()
                .hover(|d| d.bg(p.selected))
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
                        .flex_none()
                        .text_xs()
                        .text_color(p.faint_foreground)
                        .child(if self.copied {
                            "Copied".to_owned()
                        } else if released.is_some() {
                            // Its worktree is gone; its work is in the base.
                            base.map(|b| format!("in {b} · worktree removed"))
                                .unwrap_or_default()
                        } else {
                            base.map(|b| format!("from {b}")).unwrap_or_default()
                        }),
                )
                .children(restore)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(name) = &copy {
                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(name.clone()));
                        this.copied = true;
                        cx.notify();
                    }
                })),
        );

        // Its pull request: what it is and how its checks stand. Opens in
        // the browser.
        let pr = pull_request(self.store.read(cx), &self.thread_id);
        if place == Place::Window && pr.is_some() != self.pr_row {
            // Found (or gone) while the card is open: its window follows.
            self.pr_row = pr.is_some();
            let grow = if self.pr_row { TALL_ROW } else { -TALL_ROW };
            let now = window.bounds().size;
            window.resize(size(now.width, now.height + px(grow)));
        }
        if let Some(pr) = pr {
            use crate::pull_request as show;
            let tone = show::color(show::tone(&pr), &p);
            let url = pr.url.clone();
            // Under the title, while it is open: "3 passing, 1 failing, 2
            // running", each count in its own colour, then the review (the
            // part that gives way when the line is full). Nothing to say:
            // the title has the row.
            let counts: Vec<_> = [
                (pr.checks.passing, "passing", p.success),
                (pr.checks.failing, "failing", p.danger),
                (pr.checks.pending, "running", p.warning),
            ]
            .into_iter()
            .filter(|(n, ..)| *n > 0 && pr.is_open())
            .collect();
            let review = pr.review.filter(|_| pr.is_open());
            let status = (!counts.is_empty() || review.is_some()).then(|| {
                let mut line = h_flex()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(p.faint_foreground);
                let last = counts.len().saturating_sub(1);
                for (ix, (n, what, color)) in counts.iter().enumerate() {
                    line = line.child(
                        h_flex()
                            .flex_none()
                            .child(div().text_color(*color).child(format!("{n} {what}")))
                            .when(ix < last, |d| d.child(",")),
                    );
                }
                if let Some(review) = review {
                    line = line
                        .when(!counts.is_empty(), |d| d.child("·"))
                        .child(div().min_w_0().truncate().child(show::review_label(review)));
                }
                line
            });
            // All of it in words, for when the row cuts a long line short.
            let about = format!("#{} {} · {}", pr.number, pr.title, show::describe(&pr));
            top = top.child(
                row("details-pull-request", &p)
                    .h(px(TALL_ROW))
                    .cursor_pointer()
                    .hover(|d| d.bg(p.selected))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(about.clone()).build(window, cx)
                    })
                    .child(
                        Icon::new(show::icon(&pr))
                            .small()
                            .text_color(tone)
                            .into_any_element(),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(p.muted_foreground)
                                            .child(format!("#{}", pr.number)),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().truncate().child(pr.title.clone()),
                                    )
                                    // Open, Draft, Merged or Closed.
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(p.faint_foreground)
                                            .child(show::state_label(pr.state)),
                                    ),
                            )
                            .children(status),
                    )
                    .on_click(move |_, window, cx| {
                        cx.open_url(&url);
                        Self::dismiss(place, window, cx);
                    }),
            );
        }

        // Changes not on the remote yet.
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
        top = top.child(
            row("details-changes", &p)
                .cursor_pointer()
                .hover(|d| d.bg(p.selected))
                .child(icon(IconName::FileDiff, &p))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .child("Changes")
                        .when_some(self.changes_base.clone(), |d, base| {
                            d.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .child(format!("vs {base}")),
                            )
                        }),
                )
                .child(changes)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.in_main(window, cx, |w, window, cx| {
                        w.open_tab(Tab::Changes, window, cx)
                    });
                })),
        );

        // Where it came from.
        if let Some((parent, title)) = self.info.parent.clone() {
            let from = match parent.kind {
                ForkKind::Spawned => format!("Started by {title}"),
                _ => format!("Forked from {title}"),
            };
            top = top.child(divider(&p)).child(
                row("details-parent", &p)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.selected))
                    .child(icon(IconName::GitFork, &p))
                    .child(div().flex_1().min_w_0().truncate().child(from))
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

        let mut lists = v_flex()
            .id("details-list")
            .min_h_0()
            .max_h(px(LISTS_MAX))
            .overflow_y_scroll();
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
        let agents_listed = !agents.is_empty();
        if agents_listed {
            let model_state = self.store.read(cx).threads.get(&self.thread_id).cloned();
            // The line under what stays put is not part of what scrolls.
            top = top.child(divider(&p));
            lists = lists.child(section("Subagents", &p));
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
                lists = lists.child(
                    row(&format!("details-agent-{}", agent.id), &p)
                        .group("details-agent")
                        .cursor_pointer()
                        .hover(|d| d.bg(p.selected))
                        .child(crate::items::agent_avatar(
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
                                    .claims_press()
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

        // Threads its agent started: what each is doing, and a way to it.
        let threads = started_threads(self.store.read(cx), &self.thread_id);
        if !threads.is_empty() {
            if agents_listed {
                lists = lists.child(divider(&p));
            } else {
                top = top.child(divider(&p));
            }
            lists = lists.child(section("Threads", &p));
        }
        for thread in threads {
            let running = thread.status == ThreadStatus::Running;
            let (dot, note) = match thread.status {
                ThreadStatus::Running => (p.accent, None),
                ThreadStatus::WaitingApproval => (p.warning, Some("Approve")),
                ThreadStatus::Error => (p.danger, Some("Failed")),
                _ if thread.is_waiting() => (p.accent, Some("Waiting")),
                _ => (p.success, None),
            };
            let open_id = thread.id.clone();
            lists = lists.child(
                row(&format!("details-thread-{}", thread.id.0), &p)
                    .cursor_pointer()
                    .hover(|d| d.bg(p.selected))
                    .child(crate::items::agent_avatar(
                        thread.harness.as_str(),
                        22.,
                        dot,
                        running,
                        &p,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(thread.title.clone()),
                    )
                    .children(note.map(|note| div().text_xs().text_color(dot).child(note)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let (store, id) = (this.store.clone(), open_id.clone());
                        this.in_main(window, cx, move |_, _, cx| {
                            store.update(cx, |s, cx| s.select_thread(id, cx));
                        });
                    })),
            );
        }

        let card = v_flex()
            .p(px(PAD))
            .child(top)
            .child(lists)
            .bg(p.elevated)
            .border_1()
            .border_color(p.border)
            .rounded(px(12.))
            .font_weight(FontWeight::NORMAL);
        match place {
            // No taller than its column either.
            // It floats over the conversation's margin when there is one
            // conversation: what is under it gets no pointer, or the
            // conversation, which takes the wheel before anything over it,
            // scrolls instead of the card's lists.
            Place::Docked => card.w_full().min_h_0().occlude().into_any_element(),
            Place::Window => div()
                .size_full()
                .track_focus(&self.focus)
                .on_key_down(|event, window, _| {
                    if event.keystroke.key == "escape" {
                        window.remove_window();
                    }
                })
                .child(card.size_full().shadow_lg())
                .into_any_element(),
            Place::Overlay => {
                // As tall as what it lists, within the window.
                let at = cx.try_global::<Overlay>().map_or(0., |o| f32::from(o.at.y));
                let room = f32::from(window.viewport_size().height) - at - 8.;
                div()
                    .w_full()
                    .track_focus(&self.focus)
                    .on_key_down(|event, _, cx| {
                        if event.keystroke.key == "escape" {
                            close_overlay(cx);
                        }
                    })
                    .child(card.w_full().max_h(px(room.max(TALL_ROW))).shadow_lg())
                    .into_any_element()
            }
        }
    }
}
