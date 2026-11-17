//! File explorer: a lazily loaded tree of the thread's working directory,
//! rendered as a virtualized flat list.

use crate::spinner::Spinner;
use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Context, ElementId, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use loft_protocol::{DirEntry, FsRoot, Request, Response};
use std::collections::{HashMap, HashSet};

pub enum FileTreeEvent {
    Open(String),
}

enum DirState {
    Loading,
    Loaded(Vec<DirEntry>),
    Failed(String),
}

pub struct FileTree {
    store: Entity<AppStore>,
    root: FsRoot,
    dirs: HashMap<String, DirState>,
    expanded: HashSet<String>,
    selected: Option<String>,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<FileTreeEvent> for FileTree {}

struct Row {
    depth: usize,
    entry: Option<DirEntry>,
    /// Placeholder rows: loading / error messages under a directory.
    note: Option<SharedString>,
}

impl FileTree {
    pub fn new(store: Entity<AppStore>, root: FsRoot, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            store,
            root,
            dirs: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
        };
        this.load(String::new(), cx);
        this
    }

    fn load(&mut self, dir: String, cx: &mut Context<Self>) {
        self.dirs.insert(dir.clone(), DirState::Loading);
        let body = Request::ListDir {
            root: self.root.clone(),
            rel_path: dir.clone(),
        };
        request_in(&self.store, body, cx, move |this, result, cx| {
            let state = match result {
                Ok(Response::DirListing { entries }) => DirState::Loaded(entries),
                Ok(other) => DirState::Failed(format!("unexpected: {other:?}")),
                Err(err) => DirState::Failed(err.message),
            };
            this.dirs.insert(dir, state);
            cx.notify();
        });
        cx.notify();
    }

    /// Reload every loaded directory (after an agent turn changed files).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let dirs: Vec<String> = self.dirs.keys().cloned().collect();
        for dir in dirs {
            if dir.is_empty() || self.expanded.contains(&dir) {
                let body = Request::ListDir {
                    root: self.root.clone(),
                    rel_path: dir.clone(),
                };
                request_in(&self.store, body, cx, move |this, result, cx| {
                    if let Ok(Response::DirListing { entries }) = result {
                        this.dirs.insert(dir, DirState::Loaded(entries));
                        cx.notify();
                    }
                });
            } else {
                self.dirs.remove(&dir);
            }
        }
    }

    fn toggle(&mut self, entry: &DirEntry, cx: &mut Context<Self>) {
        self.selected = Some(entry.rel_path.clone());
        if entry.is_dir {
            if !self.expanded.remove(&entry.rel_path) {
                self.expanded.insert(entry.rel_path.clone());
                if !matches!(self.dirs.get(&entry.rel_path), Some(DirState::Loaded(_))) {
                    self.load(entry.rel_path.clone(), cx);
                }
            }
            cx.notify();
        } else {
            cx.emit(FileTreeEvent::Open(entry.rel_path.clone()));
            cx.notify();
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.push_rows("", 0, &mut rows);
        rows
    }

    fn push_rows(&self, dir: &str, depth: usize, rows: &mut Vec<Row>) {
        match self.dirs.get(dir) {
            None | Some(DirState::Loading) => rows.push(Row {
                depth,
                entry: None,
                note: Some("Loading…".into()),
            }),
            Some(DirState::Failed(err)) => rows.push(Row {
                depth,
                entry: None,
                note: Some(err.clone().into()),
            }),
            Some(DirState::Loaded(entries)) => {
                if entries.is_empty() && depth == 0 {
                    rows.push(Row {
                        depth,
                        entry: None,
                        note: Some("Empty folder".into()),
                    });
                }
                for e in entries {
                    rows.push(Row {
                        depth,
                        entry: Some(e.clone()),
                        note: None,
                    });
                    if e.is_dir && self.expanded.contains(&e.rel_path) {
                        self.push_rows(&e.rel_path, depth + 1, rows);
                    }
                }
            }
        }
    }
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let rows = self.rows();
        let count = rows.len();
        let selected = self.selected.clone();
        let expanded = self.expanded.clone();
        v_flex().size_full().child(
            uniform_list(
                "file-tree",
                count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let p = Palette::of(cx);
                    let rows = this.rows();
                    range
                        .filter_map(|ix| rows.get(ix).map(|r| (ix, r)))
                        .map(|(ix, row)| {
                            let indent = px(10. + row.depth as f32 * 14.);
                            let Some(entry) = row.entry.clone() else {
                                return h_flex()
                                    .id(ElementId::NamedInteger("note".into(), ix as u64))
                                    .h(px(26.))
                                    .pl(indent + px(18.))
                                    .gap_2()
                                    .text_xs()
                                    .text_color(p.faint_foreground)
                                    .when(row.note.as_deref() == Some("Loading…"), |d| {
                                        d.child(Spinner::new().xsmall())
                                    })
                                    .child(row.note.clone().unwrap_or_default())
                                    .into_any_element();
                            };
                            let is_open = entry.is_dir && expanded.contains(&entry.rel_path);
                            let active = selected.as_deref() == Some(entry.rel_path.as_str());
                            h_flex()
                                .id(ElementId::Name(entry.rel_path.clone().into()))
                                .h(px(26.))
                                .pl(indent)
                                .pr_2()
                                .gap_1p5()
                                .items_center()
                                .rounded(px(6.))
                                .mx_1()
                                .text_sm()
                                .cursor_pointer()
                                .when(active, |d| d.bg(p.selected))
                                .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                                .child(div().w(px(12.)).text_color(p.faint_foreground).when(
                                    entry.is_dir,
                                    |d| {
                                        d.child(
                                            Icon::new(if is_open {
                                                IconName::ChevronDown
                                            } else {
                                                IconName::ChevronRight
                                            })
                                            .xsmall(),
                                        )
                                    },
                                ))
                                .child(crate::file_icons::icon(
                                    &entry.name,
                                    entry.is_dir,
                                    is_open,
                                    px(16.),
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(entry.name.clone()),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle(&entry, cx)),
                                )
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .py_1()
            .bg(p.background),
        )
    }
}
