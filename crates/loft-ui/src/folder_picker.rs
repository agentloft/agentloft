//! Add Project without the system dialog: browse folders in place, with
//! git repositories (and their branch) called out and the places your
//! projects already live one click away.
//!
//! Keys: type to filter (or type a path: `~/dev`, `/opt`), ↑/↓ choose,
//! ⏎ open the folder, ⌘⏎ add the folder you're in, ⌫ on an empty filter
//! goes up, esc closes.

use crate::store::AppStore;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable as _,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use std::path::{Path, PathBuf};

pub enum FolderPickerEvent {
    /// Add this folder as a project.
    Pick(PathBuf),
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    /// `Some(branch)` for a git repository (`Some("")` when detached or a
    /// worktree we can't read the branch of).
    pub git: Option<String>,
}

pub struct FolderPicker {
    store: Entity<AppStore>,
    dir: PathBuf,
    entries: Vec<Entry>,
    loading: bool,
    error: Option<String>,
    query: String,
    highlighted: usize,
    search: Entity<InputState>,
    scroll: ScrollHandle,
    _load: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FolderPickerEvent> for FolderPicker {}

impl FolderPicker {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter, or type a path…"));
        let subs =
            vec![cx.subscribe_in(
                &search,
                window,
                |this, state, event, window, cx| match event {
                    InputEvent::Change => {
                        this.query = state.read(cx).value().to_string();
                        this.highlighted = 0;
                        cx.notify();
                    }
                    InputEvent::PressEnter { secondary, .. } => {
                        if *secondary {
                            this.add_current(cx);
                        } else {
                            this.open_highlighted(window, cx);
                        }
                    }
                    _ => {}
                },
            )];
        // Start where projects already live, else home.
        let start = common_parent(&store, cx).unwrap_or_else(home);
        let mut this = Self {
            store,
            dir: start.clone(),
            entries: Vec::new(),
            loading: true,
            error: None,
            query: String::new(),
            highlighted: 0,
            search,
            scroll: ScrollHandle::new(),
            _load: None,
            _subs: subs,
        };
        this.navigate(start, window, cx);
        this
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
    }

    fn navigate(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.dir = dir.clone();
        self.loading = true;
        self.error = None;
        self.highlighted = 0;
        self.query.clear();
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        let read = cx.background_spawn(async move { list_dirs(&dir) });
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(entries) => this.entries = entries,
                    Err(err) => {
                        this.entries.clear();
                        this.error = Some(err);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn visible(&self) -> Vec<&Entry> {
        let q = self.query.to_lowercase();
        if looks_like_path(&self.query) {
            return Vec::new();
        }
        let show_hidden = q.starts_with('.');
        self.entries
            .iter()
            .filter(|e| show_hidden || !e.name.starts_with('.'))
            .filter(|e| q.is_empty() || e.name.to_lowercase().contains(&q))
            .collect()
    }

    fn open_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if looks_like_path(&self.query) {
            let path = expand(&self.query);
            if path.is_dir() {
                self.navigate(path, window, cx);
            } else {
                self.error = Some(format!("No folder at {}", self.query.trim()));
                cx.notify();
            }
            return;
        }
        let target = self.visible().get(self.highlighted).map(|e| e.path.clone());
        if let Some(path) = target {
            self.navigate(path, window, cx);
        }
    }

    fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let from = self.dir.clone();
            self.navigate(parent, window, cx);
            // Land on the folder we came from once it's listed.
            self.pending_focus(from, cx);
        }
    }

    /// After a load, highlight `path` (going up keeps your place).
    fn pending_focus(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let task = self._load.take();
        self._load = Some(cx.spawn(async move |this, cx| {
            if let Some(task) = task {
                task.await;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(ix) = this.visible().iter().position(|e| e.path == path) {
                    this.highlighted = ix;
                    this.scroll.scroll_to_item(ix);
                    cx.notify();
                }
            });
        }));
    }

    fn add_current(&mut self, cx: &mut Context<Self>) {
        cx.emit(FolderPickerEvent::Pick(self.dir.clone()));
    }

    fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.visible().len() as isize;
        if n > 0 {
            self.highlighted = (self.highlighted as isize + delta).rem_euclid(n) as usize;
            self.scroll.scroll_to_item(self.highlighted);
            cx.notify();
        }
    }

    fn render_crumbs(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let home = home();
        let mut row = h_flex()
            .gap_0p5()
            .items_center()
            .min_w_0()
            .overflow_hidden();
        // `~` stands for home; everything above it is shown as-is.
        let (root_label, root, rest) = match self.dir.strip_prefix(&home) {
            Ok(rest) => ("~".to_owned(), home.clone(), rest.to_path_buf()),
            Err(_) => ("/".to_owned(), PathBuf::from("/"), {
                self.dir
                    .strip_prefix("/")
                    .unwrap_or(&self.dir)
                    .to_path_buf()
            }),
        };
        let mut crumbs = vec![(root_label, root.clone())];
        let mut acc = root;
        for part in rest.components() {
            acc = acc.join(part);
            crumbs.push((part.as_os_str().to_string_lossy().into_owned(), acc.clone()));
        }
        let last = crumbs.len() - 1;
        for (ix, (label, path)) in crumbs.into_iter().enumerate() {
            if ix > 0 {
                row = row.child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(p.faint_foreground),
                );
            }
            let current = ix == last;
            row = row.child(
                div()
                    .id(SharedString::from(format!("crumb-{ix}")))
                    .flex_none()
                    .px_1()
                    .py(px(2.))
                    .rounded(px(5.))
                    .text_sm()
                    .when(current, |d| {
                        d.text_color(p.foreground).font_weight(FontWeight::MEDIUM)
                    })
                    .when(!current, |d| {
                        d.cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(label)
                    .when(!current, |d| {
                        d.on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(path.clone(), window, cx)
                        }))
                    }),
            );
        }
        row.into_any_element()
    }

    /// Home, and the folders your projects live in.
    fn render_places(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut places: Vec<(String, PathBuf)> = vec![("Home".into(), home())];
        let store = self.store.read(cx);
        let mut parents: Vec<PathBuf> = store
            .workspace
            .projects
            .iter()
            .filter_map(|p| Path::new(&p.root_path).parent().map(Path::to_path_buf))
            .collect();
        parents.sort();
        parents.dedup();
        for parent in parents.into_iter().take(4) {
            let name = parent
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| crate::workspace::tilde_path(&parent));
            if !places.iter().any(|(_, p)| *p == parent) {
                places.push((name, parent));
            }
        }
        for name in ["Desktop", "Documents", "Developer"] {
            let path = home().join(name);
            if path.is_dir() && !places.iter().any(|(_, p)| *p == path) {
                places.push((name.to_owned(), path));
            }
        }
        let mut row = h_flex().gap_1().flex_wrap().px_3().py_2();
        for (ix, (label, path)) in places.into_iter().enumerate() {
            let here = path == self.dir;
            row = row.child(
                h_flex()
                    .id(SharedString::from(format!("place-{ix}")))
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(7.))
                    .text_xs()
                    .border_1()
                    .when(here, |d| {
                        d.border_color(p.accent)
                            .bg(p.accent_soft)
                            .text_color(p.foreground)
                    })
                    .when(!here, |d| {
                        d.border_color(p.border)
                            .text_color(p.muted_foreground)
                            .cursor_pointer()
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(
                        Icon::new(if ix == 0 {
                            IconName::House
                        } else {
                            IconName::Folder
                        })
                        .xsmall(),
                    )
                    .child(label)
                    .tooltip({
                        let full = crate::workspace::tilde_path(&path);
                        move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(full.clone())
                                .build(window, cx)
                        }
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate(path.clone(), window, cx)
                    })),
            );
        }
        row.into_any_element()
    }
}

impl Render for FolderPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_kit::component::input::{Backspace, Escape, MoveDown, MoveUp};
        let p = Palette::of(cx);
        let added: Vec<PathBuf> = self
            .store
            .read(cx)
            .workspace
            .projects
            .iter()
            .map(|p| PathBuf::from(&p.root_path))
            .collect();
        let rows: Vec<Entry> = self.visible().into_iter().cloned().collect();
        if self.highlighted >= rows.len() {
            self.highlighted = rows.len().saturating_sub(1);
        }
        let here_added = added.contains(&self.dir);
        let here_git = self.dir.join(".git").exists();

        let mut list = v_flex()
            .id("folder-list")
            .h(px(320.))
            .p_1()
            .gap_px()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        let note = if self.loading {
            Some("Reading…".to_owned())
        } else if let Some(err) = &self.error {
            Some(err.clone())
        } else if looks_like_path(&self.query) {
            Some(format!("⏎ to go to {}", self.query.trim()))
        } else if rows.is_empty() {
            Some(if self.query.is_empty() {
                "No folders here.".to_owned()
            } else {
                "No folders match.".to_owned()
            })
        } else {
            None
        };
        if let Some(note) = note {
            list = list.child(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(p.faint_foreground)
                    .child(note),
            );
        }
        for (ix, entry) in rows.iter().enumerate() {
            let active = ix == self.highlighted;
            let is_added = added.contains(&entry.path);
            let path = entry.path.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("folder-{ix}")))
                    .gap_2()
                    .items_center()
                    .h(px(32.))
                    .px_2()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |d| d.bg(p.selected))
                    .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                    .child(crate::file_icons::icon(&entry.name, true, false, px(16.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(entry.name.starts_with('.'), |d| {
                                d.text_color(p.muted_foreground)
                            })
                            .child(entry.name.clone()),
                    )
                    .when(is_added, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(p.faint_foreground)
                                .child("added"),
                        )
                    })
                    .when_some(entry.git.clone(), |d, branch| {
                        d.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .px_1p5()
                                .py(px(1.))
                                .rounded(px(5.))
                                .bg(p.accent_soft)
                                .text_xs()
                                .text_color(p.accent)
                                .child(Icon::new(IconName::GitBranch).xsmall())
                                .child(if branch.is_empty() {
                                    "git".to_owned()
                                } else {
                                    branch
                                }),
                        )
                    })
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.highlighted != ix {
                            this.highlighted = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, e: &gpui_kit::ClickEvent, window, cx| {
                            // Double-click adds the folder; a click opens it.
                            if e.click_count() >= 2 {
                                cx.emit(FolderPickerEvent::Pick(path.clone()));
                            } else {
                                this.navigate(path.clone(), window, cx);
                            }
                        }),
                    ),
            );
        }

        let add_label = if here_added {
            "Already added"
        } else {
            "Add this folder"
        };
        v_flex()
            .w(px(620.))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                cx.stop_propagation();
                this.move_by(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                cx.stop_propagation();
                this.move_by(1, cx);
            }))
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                if this.query.is_empty() {
                    cx.stop_propagation();
                    this.go_up(window, cx);
                }
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.stop_propagation();
                cx.emit(FolderPickerEvent::Close);
            }))
            // Header: up, where you are, add.
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        div()
                            .id("folder-up")
                            .p_1()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                            .child(Icon::new(IconName::ArrowLeft).small())
                            .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
                    )
                    .child(div().flex_1().min_w_0().child(self.render_crumbs(&p, cx)))
                    .child(
                        h_flex()
                            .id("folder-add")
                            .flex_none()
                            .gap_1p5()
                            .items_center()
                            .px_2p5()
                            .py_1()
                            .rounded(px(8.))
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .when(!here_added, |d| {
                                d.bg(p.accent)
                                    .text_color(p.accent_foreground)
                                    .cursor_pointer()
                                    .hover(|d| d.opacity(0.9))
                                    .on_click(cx.listener(|this, _, _, cx| this.add_current(cx)))
                            })
                            .when(here_added, |d| d.bg(p.muted).text_color(p.faint_foreground))
                            .when(here_git, |d| {
                                d.child(Icon::new(IconName::GitBranch).xsmall())
                            })
                            .child(add_label)
                            .child(div().text_xs().opacity(0.7).child("⌘⏎")),
                    ),
            )
            .child(
                div().px_3().pt_2().child(
                    Input::new(&self.search)
                        .prefix(Icon::new(IconName::Search).small())
                        .small(),
                ),
            )
            .child(self.render_places(&p, cx))
            .child(list)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.faint_foreground)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(key("↑↓", "choose", &p))
                            .child(key("⏎", "open", &p))
                            .child(key("⌫", "up", &p))
                            .child(key("esc", "close", &p)),
                    )
                    .child(
                        div()
                            .id("folder-system")
                            .cursor_pointer()
                            .hover(|d| d.text_color(p.foreground))
                            .child("System picker…")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let dir = this.dir.clone();
                                cx.emit(FolderPickerEvent::Close);
                                system_picker(this.store.clone(), dir, cx);
                            })),
                    ),
            )
    }
}

fn key(k: &'static str, what: &'static str, p: &Palette) -> AnyElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(
            div()
                .px_1()
                .rounded(px(4.))
                .bg(p.muted)
                .text_color(p.muted_foreground)
                .child(k),
        )
        .child(what)
        .into_any_element()
}

/// The macOS folder dialog, for when it's really wanted.
fn system_picker(store: Entity<AppStore>, _from: PathBuf, cx: &mut Context<FolderPicker>) {
    let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add project".into()),
    });
    cx.spawn(async move |_, cx| {
        if let Ok(Ok(Some(paths))) = rx.await
            && let Some(path) = paths.into_iter().next()
        {
            store.update(cx, |s, cx| {
                s.add_project(path.to_string_lossy().into_owned(), cx)
            });
        }
    })
    .detach();
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// The deepest folder all projects share, when there are any (and it isn't
/// just `/` or home itself).
fn common_parent(store: &Entity<AppStore>, cx: &App) -> Option<PathBuf> {
    let roots: Vec<PathBuf> = store
        .read(cx)
        .workspace
        .projects
        .iter()
        .filter_map(|p| Path::new(&p.root_path).parent().map(Path::to_path_buf))
        .collect();
    let first = roots.first()?.clone();
    let mut common = first;
    for r in &roots[1..] {
        while !r.starts_with(&common) {
            common = common.parent()?.to_path_buf();
        }
    }
    (common.is_dir() && common != Path::new("/")).then_some(common)
}

fn looks_like_path(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('/') || q.starts_with('~')
}

fn expand(query: &str) -> PathBuf {
    let q = query.trim();
    match q.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(q),
    }
}

/// Sub-folders of `dir`, git repositories marked with their branch.
pub fn list_dirs(dir: &Path) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "macOS hasn't given Agentloft access to this folder.".to_owned()
        }
        _ => format!("Can't read this folder: {e}"),
    })?;
    let mut out: Vec<Entry> = read
        .flatten()
        .filter(|e| {
            // Follow symlinks to folders; skip files.
            std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir())
        })
        .map(|e| {
            let path = e.path();
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                git: git_branch(&path),
                path,
            }
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// `Some(branch)` if `dir` is a git checkout (`Some("")` when the branch
/// can't be read, e.g. detached or a linked worktree).
fn git_branch(dir: &Path) -> Option<String> {
    let git = dir.join(".git");
    if git.is_file() {
        return Some(String::new());
    }
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    Some(
        head.trim()
            .strip_prefix("ref: refs/heads/")
            .unwrap_or_default()
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_folders_and_repos() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("repo/.git")).unwrap();
        std::fs::write(d.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(d.join("Notes")).unwrap();
        std::fs::create_dir(d.join(".hidden")).unwrap();
        std::fs::write(d.join("file.txt"), "x").unwrap();
        let entries = list_dirs(d).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".hidden", "Notes", "repo"]);
        assert_eq!(entries[2].git.as_deref(), Some("main"));
        assert_eq!(entries[1].git, None);
    }

    #[test]
    fn paths_typed_into_the_filter() {
        assert!(looks_like_path("~/dev"));
        assert!(looks_like_path(" /opt"));
        assert!(!looks_like_path("dev"));
        assert_eq!(expand("~/dev"), home().join("dev"));
        assert_eq!(expand("~"), home());
    }
}
