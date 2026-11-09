//! The thread's details card (title-bar button): where it runs, open in
//! an editor, its branch and changes, where it was forked from. It opens
//! in a small floating window so it shows over the browser panel too.

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

fn installed_editors() -> Vec<usize> {
    let home = std::env::home_dir();
    EDITORS
        .iter()
        .enumerate()
        .filter(|(_, (app, _))| {
            let name = format!("{app}.app");
            [
                Some(PathBuf::from("/Applications")),
                Some(PathBuf::from("/System/Applications/Utilities")),
                home.as_ref().map(|h| h.join("Applications")),
            ]
            .into_iter()
            .flatten()
            .any(|dir| dir.join(&name).exists())
        })
        .map(|(ix, _)| ix)
        .collect()
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
    let rows = 3. + if info.parent.is_some() { 1. } else { 0. };
    let dividers = 1. + if info.parent.is_some() { 1. } else { 0. };
    let height = PAD * 2. + TALL_ROW + rows * ROW + dividers * DIVIDER + 4.;
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
        cx.new(|cx| DetailsCard::new(store, workspace, main, thread_id, info, window, cx))
    });
}

struct DetailsCard {
    store: gpui_kit::Entity<AppStore>,
    workspace: WeakEntity<WorkspaceView>,
    main: AnyWindowHandle,
    info: Info,
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
    fn new(
        store: gpui_kit::Entity<AppStore>,
        workspace: WeakEntity<WorkspaceView>,
        main: AnyWindowHandle,
        thread_id: ThreadId,
        info: Info,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.observe_window_activation(window, |_, window, _| {
            if !window.is_window_active() {
                window.remove_window();
            }
        })
        .detach();
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
        if let Some(client) = store.read(cx).client() {
            let body = Request::GetDiff {
                thread_id: thread_id.clone(),
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
        Self {
            store,
            workspace,
            main,
            info,
            focus,
            computer: None,
            local_branch: None,
            changes: None,
            editors: installed_editors(),
            editor_anchor: Default::default(),
            copied: false,
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
                            window.remove_window();
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
                    .child(icon(IconName::Code, &p))
                    .child(div().flex_1().child(format!("Open in {}", EDITORS[ed].1)))
                    .on_click(move |_, window, _| {
                        open_in(ed, &folder);
                        window.remove_window();
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

        div()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(|event, window, _| {
                if event.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
            .child(
                list.size_full()
                    .bg(p.elevated)
                    .border_1()
                    .border_color(p.border)
                    .rounded(px(12.))
                    .shadow_lg()
                    .font_weight(FontWeight::NORMAL),
            )
    }
}
