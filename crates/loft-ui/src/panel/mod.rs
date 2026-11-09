//! The workbench panel beside the conversation, per thread: one strip of
//! tabs of any kind (files, changes, terminals, browser pages, open files,
//! subagents) and a + menu to open more.

pub mod agent;
pub mod changes;
pub mod docs;
pub mod files;
pub mod terminal;
pub mod viewer;

use crate::store::{AppStore, ThreadState};
use crate::theme::Palette;
use agent::AgentView;
use changes::ChangesView;
use files::{FileTree, FileTreeEvent};
use gpui_kit::assets::IconName;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Bounds, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use loft_browser::BrowserView;
use loft_protocol::{FsRoot, ItemId, Request, Response, ThreadId, TurnId};
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use terminal::TerminalPane;
use viewer::FileViewer;

/// A kind of panel tab (title-bar shortcuts, the palette, the + menu).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    Files,
    Changes,
    Terminal,
    Browser,
    File(String),
    Agent(ItemId),
}

/// An open tab.
enum Open {
    Files,
    Changes,
    Terminal {
        key: String,
        pane: Entity<TerminalPane>,
    },
    Browser {
        view: Entity<BrowserView>,
    },
    File {
        path: String,
        viewer: Entity<FileViewer>,
    },
    Agent {
        id: ItemId,
        view: Entity<AgentView>,
    },
}

impl Open {
    fn kind(&self) -> Tab {
        match self {
            Open::Files => Tab::Files,
            Open::Changes => Tab::Changes,
            Open::Terminal { .. } => Tab::Terminal,
            Open::Browser { .. } => Tab::Browser,
            Open::File { path, .. } => Tab::File(path.clone()),
            Open::Agent { id, .. } => Tab::Agent(id.clone()),
        }
    }
}

struct ThreadPanel {
    files: Entity<FileTree>,
    changes: Entity<ChangesView>,
    tabs: Vec<Open>,
    active: usize,
    /// Shells left running in the daemon (after a restart) were reattached.
    terminals_restored: bool,
    /// Last finished turn we refreshed for.
    seen_turn: Option<TurnId>,
    /// The thread's working directory; changes when its worktree is created.
    cwd: String,
    _subs: Vec<Subscription>,
}

impl ThreadPanel {
    fn active_tab(&self) -> Option<&Open> {
        self.tabs.get(self.active)
    }

    /// The tab of `kind` to show: the active one if it is that kind, else
    /// the last opened.
    fn find(&self, kind: &Tab) -> Option<usize> {
        if self.active_tab().is_some_and(|t| t.kind() == *kind) {
            return Some(self.active);
        }
        self.tabs.iter().rposition(|t| t.kind() == *kind)
    }

    fn browser(&self) -> Option<Entity<BrowserView>> {
        let ix = self.find(&Tab::Browser)?;
        match &self.tabs[ix] {
            Open::Browser { view } => Some(view.clone()),
            _ => None,
        }
    }
}

/// What the panel tells the workspace.
pub enum PanelEvent {
    /// A note left on a page in `thread`'s browser.
    Annotation(ThreadId, loft_browser::Annotation),
    /// A screenshot taken in `thread`'s browser (PNG), for its composer.
    Screenshot(ThreadId, Vec<u8>),
    /// The panel was expanded to fill the window, or brought back.
    Maximized(bool),
}

pub struct WorkbenchPanel {
    store: Entity<AppStore>,
    threads: HashMap<ThreadId, ThreadPanel>,
    /// Where the + button is (its menu opens under it).
    add_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The tab strip's scroll, kept on the active tab.
    tab_scroll: gpui_kit::ScrollHandle,
    shown: Option<(ThreadId, usize)>,
    /// Filling the window (the conversation is hidden).
    pub maximized: bool,
}

impl gpui_kit::EventEmitter<PanelEvent> for WorkbenchPanel {}

impl WorkbenchPanel {
    pub fn new(store: Entity<AppStore>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            threads: HashMap::new(),
            add_anchor: Rc::default(),
            tab_scroll: gpui_kit::ScrollHandle::new(),
            shown: None,
            maximized: false,
        }
    }

    fn selected(&self, cx: &gpui_kit::App) -> Option<(ThreadId, Entity<ThreadState>)> {
        let store = self.store.read(cx);
        let id = store.selected.clone()?;
        let entity = store.threads.get(&id)?.clone();
        Some((id, entity))
    }

    fn ensure(
        &mut self,
        thread_id: &ThreadId,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> &mut ThreadPanel {
        if !self.threads.contains_key(thread_id) {
            let root = FsRoot::Thread(thread_id.clone());
            let files = cx.new(|cx| FileTree::new(self.store.clone(), root, cx));
            let changes = cx.new(|cx| ChangesView::new(self.store.clone(), thread_id.clone(), cx));
            let id = thread_id.clone();
            let subs = vec![
                cx.subscribe_in(&files, window, {
                    let id = id.clone();
                    move |this, _, event, window, cx| match event {
                        FileTreeEvent::Open(path) => this.open_file(&id, path.clone(), window, cx),
                    }
                }),
                // Refresh files and changes when a turn finishes.
                cx.observe_in(thread, window, move |this, thread, window, cx| {
                    this.on_thread_changed(&id, &thread, window, cx)
                }),
            ];
            let model = thread.read(cx).model.as_ref();
            let seen_turn = model
                .and_then(|m| m.last_turn())
                .filter(|t| t.status.is_terminal())
                .map(|t| t.id.clone());
            let cwd = model.map(|m| m.thread.cwd.clone()).unwrap_or_default();
            self.threads.insert(
                thread_id.clone(),
                ThreadPanel {
                    files,
                    changes,
                    tabs: Vec::new(),
                    active: 0,
                    terminals_restored: false,
                    seen_turn,
                    cwd,
                    _subs: subs,
                },
            );
        }
        self.threads.get_mut(thread_id).expect("inserted above")
    }

    fn ensure_id(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<&mut ThreadPanel> {
        let thread = self.store.read(cx).threads.get(thread_id).cloned()?;
        Some(self.ensure(thread_id, &thread, window, cx))
    }

    fn on_thread_changed(
        &mut self,
        id: &ThreadId,
        thread: &Entity<ThreadState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(model) = thread.read(cx).model.as_ref() else {
            return;
        };
        let finished = model
            .last_turn()
            .filter(|t| t.status.is_terminal())
            .map(|t| t.id.clone());
        let cwd = model.thread.cwd.clone();
        let Some(panel) = self.threads.get_mut(id) else {
            return;
        };
        // The thread moved into its new worktree: re-root everything.
        let moved = !panel.cwd.is_empty() && panel.cwd != cwd;
        panel.cwd = cwd;
        if moved {
            for tab in &panel.tabs {
                if let Open::Terminal { pane, .. } = tab {
                    pane.update(cx, |p, cx| p.restart(cx));
                }
            }
        }
        if moved || (finished.is_some() && finished != panel.seen_turn) {
            if finished.is_some() {
                panel.seen_turn = finished;
            }
            panel.files.update(cx, |f, cx| f.refresh(cx));
            panel.changes.update(cx, |c, cx| c.refresh(cx));
            for tab in &panel.tabs {
                if let Open::File { viewer, .. } = tab {
                    viewer.update(cx, |v, cx| v.reload(window, cx));
                }
            }
        }
    }

    // -- browser ---------------------------------------------------------------

    /// The selected thread's browser tab (opened if it has none).
    pub fn browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<BrowserView>> {
        let (id, _) = self.selected(cx)?;
        self.browser_for(&id, window, cx)
    }

    /// `thread_id`'s browser tab for agents: the one showing, else the last
    /// opened, else a new one.
    pub fn browser_for(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<BrowserView>> {
        if let Some(view) = self.ensure_id(thread_id, window, cx)?.browser() {
            return Some(view);
        }
        Some(self.new_browser(thread_id, window, cx))
    }

    /// `thread_id`'s browser tab, if it has one.
    pub fn existing_browser(
        &self,
        thread_id: &ThreadId,
        _cx: &gpui_kit::App,
    ) -> Option<Entity<BrowserView>> {
        self.threads.get(thread_id)?.browser()
    }

    fn new_browser(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserView> {
        let view = cx.new(|cx| BrowserView::new(window, cx));
        let id = thread_id.clone();
        let subs = [cx.subscribe(&view, move |_, _, event, cx| match event {
            loft_browser::BrowserEvent::Annotation(note) => {
                cx.emit(PanelEvent::Annotation(id.clone(), note.clone()))
            }
            loft_browser::BrowserEvent::Screenshot(png) => {
                cx.emit(PanelEvent::Screenshot(id.clone(), png.clone()))
            }
            // Titles and loading state show on the tab.
            _ => cx.notify(),
        })];
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel._subs.extend(subs);
            panel.tabs.push(Open::Browser { view: view.clone() });
            panel.active = panel.tabs.len() - 1;
        }
        cx.notify();
        view
    }

    // -- terminals -------------------------------------------------------------

    fn new_terminal(&mut self, thread_id: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let base = format!("thread:{}", thread_id.as_str());
        let Some(panel) = self.threads.get(thread_id) else {
            return;
        };
        let used: Vec<&str> = panel
            .tabs
            .iter()
            .filter_map(|t| match t {
                Open::Terminal { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        let key = if !used.contains(&base.as_str()) {
            base.clone()
        } else {
            let next = used
                .iter()
                .filter_map(|k| k.rsplit_once('#').and_then(|(_, n)| n.parse::<u32>().ok()))
                .max()
                .unwrap_or(1)
                + 1;
            format!("{base}#{next}")
        };
        let restore = !panel.terminals_restored;
        self.push_terminal(thread_id, key, window, cx);
        if restore {
            self.restore_terminals(thread_id, base, window, cx);
        }
    }

    fn push_terminal(
        &mut self,
        thread_id: &ThreadId,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let root = FsRoot::Thread(thread_id.clone());
        let pane = cx.new(|cx| TerminalPane::new(store, root, key.clone(), window, cx));
        let sub = cx.observe(&pane, |_, _, cx| cx.notify());
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel._subs.push(sub);
            panel.tabs.push(Open::Terminal {
                key,
                pane: pane.clone(),
            });
            panel.active = panel.tabs.len() - 1;
        }
        let view = pane.read(cx).view.clone();
        window.focus(&gpui_kit::Focusable::focus_handle(&view, cx), cx);
        cx.notify();
    }

    /// Extra shells still running in the daemon (after a UI restart) come
    /// back as tabs.
    fn restore_terminals(
        &mut self,
        thread_id: &ThreadId,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel.terminals_restored = true;
        }
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let body = Request::ListTerminals {
            key_prefix: format!("{base}#"),
        };
        let id = thread_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Response::Terminals { terminals }) = client.request(body).await {
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(panel) = this.threads.get(&id) else {
                        return;
                    };
                    let active = panel.active;
                    let open: Vec<String> = panel
                        .tabs
                        .iter()
                        .filter_map(|t| match t {
                            Open::Terminal { key, .. } => Some(key.clone()),
                            _ => None,
                        })
                        .collect();
                    for key in terminals.into_iter().filter_map(|t| t.key) {
                        if !open.contains(&key) {
                            this.push_terminal(&id, key, window, cx);
                        }
                    }
                    if let Some(panel) = this.threads.get_mut(&id) {
                        panel.active = active;
                    }
                });
            }
        })
        .detach();
    }

    // -- tabs ------------------------------------------------------------------

    /// Show a tab of `kind` for the selected thread: the one already open,
    /// else a new one.
    pub fn show(&mut self, kind: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, thread)) = self.selected(cx) else {
            return;
        };
        let panel = self.ensure(&id, &thread, window, cx);
        match panel.find(&kind) {
            Some(ix) => self.activate(&id, ix, window, cx),
            None => self.add(&id, kind, window, cx),
        }
    }

    /// Open a new tab of `kind` (Files and Changes exist once).
    fn add(
        &mut self,
        thread_id: &ThreadId,
        kind: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.ensure_id(thread_id, window, cx) else {
            return;
        };
        match kind {
            Tab::Files | Tab::Changes => {
                if let Some(ix) = panel.tabs.iter().position(|t| t.kind() == kind) {
                    self.activate(thread_id, ix, window, cx);
                    return;
                }
                panel.tabs.push(if kind == Tab::Files {
                    Open::Files
                } else {
                    Open::Changes
                });
                panel.active = panel.tabs.len() - 1;
            }
            Tab::Terminal => self.new_terminal(thread_id, window, cx),
            Tab::Browser => {
                let view = self.new_browser(thread_id, window, cx);
                view.update(cx, |b, cx| b.focus_address_bar(window, cx));
            }
            Tab::File(path) => self.open_file(thread_id, path, window, cx),
            Tab::Agent(id) => self.open_agent(thread_id, id, window, cx),
        }
        cx.notify();
    }

    fn activate(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        if ix >= panel.tabs.len() {
            return;
        }
        // Notes are per page: stop annotating the page we leave.
        if ix != panel.active
            && let Some(Open::Browser { view }) = panel.tabs.get(panel.active)
        {
            view.update(cx, |b, cx| b.set_annotating(false, cx));
        }
        panel.active = ix;
        if let Open::Terminal { pane, .. } = &panel.tabs[ix] {
            let view = pane.read(cx).view.clone();
            window.focus(&gpui_kit::Focusable::focus_handle(&view, cx), cx);
        }
        cx.notify();
    }

    fn close(&mut self, thread_id: &ThreadId, ix: usize, cx: &mut Context<Self>) {
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        if ix >= panel.tabs.len() {
            return;
        }
        let tab = panel.tabs.remove(ix);
        if let Open::Terminal { pane, .. } = &tab {
            pane.update(cx, |p, cx| p.close(cx));
        }
        if panel.active > ix || panel.active >= panel.tabs.len() {
            panel.active = panel.active.saturating_sub(1);
        }
        cx.notify();
    }

    pub fn open_file(
        &mut self,
        thread_id: &ThreadId,
        rel_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let Some(panel) = self.ensure_id(thread_id, window, cx) else {
            return;
        };
        let at = panel
            .tabs
            .iter()
            .position(|t| matches!(t, Open::File { path, .. } if *path == rel_path));
        panel.active = match at {
            Some(ix) => ix,
            None => {
                let root = FsRoot::Thread(thread_id.clone());
                let path = rel_path.clone();
                let viewer = cx.new(|cx| FileViewer::new(store, root, path, window, cx));
                panel.tabs.push(Open::File {
                    path: rel_path,
                    viewer,
                });
                panel.tabs.len() - 1
            }
        };
        cx.notify();
    }

    pub fn open_agent(
        &mut self,
        thread_id: &ThreadId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let Some(panel) = self.ensure_id(thread_id, window, cx) else {
            return;
        };
        let at = panel
            .tabs
            .iter()
            .position(|t| matches!(t, Open::Agent { id, .. } if *id == item_id));
        panel.active = match at {
            Some(ix) => ix,
            None => {
                let (t, i) = (thread_id.clone(), item_id.clone());
                let view = cx.new(|cx| AgentView::new(store, t, i, cx));
                panel.tabs.push(Open::Agent { id: item_id, view });
                panel.tabs.len() - 1
            }
        };
        cx.notify();
    }

    pub fn set_maximized(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.maximized != on {
            self.maximized = on;
            cx.emit(PanelEvent::Maximized(on));
            cx.notify();
        }
    }

    /// The kind of tab showing for the selected thread.
    pub fn active_tab(&self, cx: &gpui_kit::App) -> Option<Tab> {
        let id = self.store.read(cx).selected.clone()?;
        self.threads.get(&id)?.active_tab().map(Open::kind)
    }

    /// The + menu: open a new tab of any kind.
    fn add_menu(&self, thread_id: &ThreadId, window: &Window, cx: &mut Context<Self>) {
        use loft_browser::menu::{self, Entry};
        let Some(at) = self.add_anchor.get() else {
            return;
        };
        let (panel, handle) = (cx.weak_entity(), window.window_handle());
        let item = |label: &str, icon: IconName, key: char, kind: Tab| {
            let (panel, id) = (panel.clone(), thread_id.clone());
            Entry::item(label.to_owned(), icon, move |cx| {
                let (panel, id, kind) = (panel.clone(), id.clone(), kind.clone());
                let _ = handle.update(cx, move |_, window, cx| {
                    _ = panel.update(cx, |p, cx| p.add(&id, kind, window, cx));
                });
            })
            .shortcut(key)
        };
        let entries = vec![
            item("Browser", IconName::Globe, 'b', Tab::Browser),
            item("Terminal", IconName::SquareTerminal, 't', Tab::Terminal),
            item("Files", IconName::FolderTree, 'f', Tab::Files),
            item("Changes", IconName::GitCompare, 'd', Tab::Changes),
        ];
        menu::open(window, at, entries, cx);
    }
}

/// "github.com" for a URL, for untitled pages.
fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.trim_start_matches("www.").to_owned())
}

/// A shell's "user@host:~/dev/app" title as just "app".
fn terminal_title(title: &str) -> String {
    match title.split_once(':') {
        Some((who, path)) if who.contains('@') => {
            let path = path.trim().trim_end_matches('/');
            match path.rsplit('/').next() {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => path.to_owned(),
            }
        }
        _ => title.to_owned(),
    }
}

/// A tab's icon and label.
fn tab_face(tab: &Open, p: &Palette, cx: &gpui_kit::App) -> (AnyElement, String) {
    let icon = |name: IconName| Icon::new(name).xsmall().into_any_element();
    match tab {
        Open::Files => (icon(IconName::FolderTree), "Files".into()),
        Open::Changes => (icon(IconName::GitCompare), "Changes".into()),
        Open::Terminal { pane, .. } => (
            icon(IconName::SquareTerminal),
            terminal_title(&pane.read(cx).label()),
        ),
        Open::Browser { view } => {
            let b = view.read(cx);
            let title = b
                .title()
                .filter(|t| !t.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| b.url().and_then(host))
                .unwrap_or_else(|| "New Tab".into());
            let face = if b.is_loading() {
                Spinner::new()
                    .xsmall()
                    .color(p.muted_foreground)
                    .into_any_element()
            } else {
                icon(IconName::Globe)
            };
            (face, title)
        }
        Open::File { path, .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            (crate::file_icons::icon(&name, false, false, px(14.)), name)
        }
        Open::Agent { view, .. } => {
            let view = view.read(cx);
            let face = match view.harness(cx) {
                Some(h) => crate::brand::agent_logo(&h, px(13.), p),
                None => icon(IconName::Bot),
            };
            (face, view.title(cx))
        }
    }
}

impl Render for WorkbenchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let Some((thread_id, thread)) = self.selected(cx) else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .bg(p.background)
                .text_sm()
                .text_color(p.muted_foreground)
                .child("Select a thread to see its files, changes and tools.")
                .into_any_element();
        };
        if thread.read(cx).model.is_none() {
            return div().size_full().bg(p.background).into_any_element();
        }
        let tab_scroll = self.tab_scroll.clone();
        let panel = self.ensure(&thread_id, &thread, window, cx);
        let active = panel.active;

        let mut tabs = h_flex()
            .id("panel-tabs")
            .flex_shrink(1.)
            .min_w_0()
            .h_full()
            .gap_1()
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&tab_scroll);
        for (ix, tab) in panel.tabs.iter().enumerate() {
            let is_active = ix == active;
            let (icon, label) = tab_face(tab, &p, cx);
            let (tid, cid) = (thread_id.clone(), thread_id.clone());
            tabs = tabs.child(
                h_flex()
                    .id(SharedString::from(format!("panel-tab-{ix}")))
                    .group("panel-tab")
                    .flex_shrink(1.)
                    .w(px(180.))
                    .min_w(px(76.))
                    .h(px(26.))
                    .pl_2()
                    .pr_1()
                    .gap_1p5()
                    .items_center()
                    .rounded(px(7.))
                    .text_xs()
                    .cursor_pointer()
                    .when(is_active, |d| {
                        d.bg(p.background)
                            .text_color(p.foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .border_1()
                            .border_color(p.border)
                    })
                    .when(!is_active, |d| {
                        d.text_color(p.muted_foreground)
                            .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    })
                    .child(div().flex_none().child(icon))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .id(SharedString::from(format!("panel-tab-x-{ix}")))
                            .flex_none()
                            .cursor_pointer()
                            .p(px(2.))
                            .rounded(px(4.))
                            .when(!is_active, |d| {
                                d.invisible().group_hover("panel-tab", |d| d.visible())
                            })
                            .hover(|d| d.bg(p.border))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close(&cid, ix, cx);
                            })),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(&tid, ix, window, cx)),
                    ),
            );
        }
        // A new or picked tab scrolls into view.
        let shown = Some((thread_id.clone(), active));
        if self.shown != shown {
            self.shown = shown;
            self.tab_scroll.scroll_to_item(active);
        }
        let anchor = self.add_anchor.clone();
        let add_id = thread_id.clone();
        let add = div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id("panel-tab-add")
                    .p(px(5.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(IconName::Plus).small())
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.add_menu(&add_id, window, cx)),
                    ),
            )
            .child(
                canvas(
                    move |bounds, _, _| anchor.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        let strip = h_flex()
            .h(px(38.))
            .flex_shrink_0()
            .px_1p5()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .bg(p.sidebar)
            .child(tabs)
            .child(add)
            .child(div().flex_1())
            .child(
                div()
                    .id("panel-maximize")
                    .flex_none()
                    .p(px(5.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(p.muted_foreground)
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(
                        Icon::new(if self.maximized {
                            IconName::Minimize2
                        } else {
                            IconName::Maximize2
                        })
                        .small(),
                    )
                    .tooltip({
                        let label = if self.maximized {
                            "Back beside the conversation"
                        } else {
                            "Fill the window"
                        };
                        move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let on = !this.maximized;
                        this.set_maximized(on, cx);
                    })),
            );

        let panel = self.threads.get(&thread_id).expect("ensured");
        let body: AnyElement = match panel.active_tab() {
            Some(Open::Files) => panel.files.clone().into_any_element(),
            Some(Open::Changes) => panel.changes.clone().into_any_element(),
            Some(Open::Terminal { pane, .. }) => pane.clone().into_any_element(),
            Some(Open::Browser { view }) => view.clone().into_any_element(),
            Some(Open::File { viewer, .. }) => viewer.clone().into_any_element(),
            Some(Open::Agent { view, .. }) => view.clone().into_any_element(),
            None => empty_state(&thread_id, &p, cx),
        };
        v_flex()
            .size_full()
            .bg(p.background)
            .child(strip)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

/// No tabs open: offer the kinds.
fn empty_state(thread_id: &ThreadId, p: &Palette, cx: &mut Context<WorkbenchPanel>) -> AnyElement {
    let mut list = v_flex().gap_1().w(px(240.));
    for (kind, icon, label, key) in [
        (Tab::Browser, IconName::Globe, "Browser", "B"),
        (Tab::Terminal, IconName::SquareTerminal, "Terminal", "T"),
        (Tab::Files, IconName::FolderTree, "Files", "F"),
        (Tab::Changes, IconName::GitCompare, "Changes", "D"),
    ] {
        let id = thread_id.clone();
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("panel-empty-{label}")))
                .h(px(34.))
                .px_3()
                .gap_2()
                .items_center()
                .rounded(px(8.))
                .text_sm()
                .cursor_pointer()
                .text_color(p.foreground)
                .hover(|d| d.bg(p.hover))
                .child(Icon::new(icon).small().text_color(p.muted_foreground))
                .child(div().flex_1().child(label))
                .child(div().text_xs().text_color(p.faint_foreground).child(key))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.add(&id, kind.clone(), window, cx)),
                ),
        );
    }
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .child(
            div()
                .text_sm()
                .text_color(p.muted_foreground)
                .child("Open a tab"),
        )
        .child(list)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{host, terminal_title};

    #[test]
    fn terminal_tabs_show_the_folder() {
        assert_eq!(terminal_title("me@box:~/dev/app"), "app");
        assert_eq!(terminal_title("me@box:~"), "~");
        assert_eq!(terminal_title("vim notes.md"), "vim notes.md");
    }

    #[test]
    fn hosts_for_untitled_tabs() {
        assert_eq!(
            host("https://www.youtube.com/watch?v=1").as_deref(),
            Some("youtube.com")
        );
        assert_eq!(
            host("http://localhost:3000/").as_deref(),
            Some("localhost:3000")
        );
        assert_eq!(host("about:blank"), None);
    }
}
