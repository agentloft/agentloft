//! The workbench panel beside the conversation, per thread: one strip of
//! tabs of any kind (files, changes, terminals, browser pages, open files,
//! subagents) and a + menu to open more.

pub mod agent;
pub mod changes;
pub mod dock;
pub mod docs;
pub mod files;
pub mod page;
pub mod slides;
pub mod terminal;
pub mod viewer;

use crate::press::ClaimsPress as _;
use crate::spinner::Spinner;
use crate::store::{AppStore, ThreadState};
use crate::theme::Palette;
use agent::AgentView;
use changes::ChangesView;
use files::{FileTree, FileTreeEvent};
use gpui_kit::assets::IconName;
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
    /// A page of the daemon's own browser, streamed (the browser tab where
    /// there is no native web view: the web client).
    Page {
        view: Entity<page::PageView>,
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
            Open::Browser { .. } | Open::Page { .. } => Tab::Browser,
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
    /// Shells docked under the conversation (Ctrl+`), by reattach key.
    dock: Vec<(String, Entity<TerminalPane>)>,
    dock_active: usize,
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

    /// Reattach keys of every shell open, in the side panel or the dock.
    fn terminal_keys(&self) -> Vec<String> {
        let tabs = self.tabs.iter().filter_map(|t| match t {
            Open::Terminal { key, .. } => Some(key.clone()),
            _ => None,
        });
        tabs.chain(self.dock.iter().map(|(key, _)| key.clone()))
            .collect()
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
    /// A terminal moved into the side panel: show it.
    ShowPanel,
    /// A terminal moved under the conversation: show the dock.
    ShowDock,
    /// Hide the dock (its last shell closed or moved away, or asked to).
    HideDock,
    /// `thread`'s browser left annotation mode.
    AnnotatingStopped(ThreadId),
    /// A note left on a document open in `thread`'s panel: the file's path
    /// as its tab has it, and the note.
    DocNote(ThreadId, String, docs::DocNote),
    /// Text selected in one of `thread`'s terminals, for its composer.
    AddToMessage(ThreadId, String),
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
    /// The window has no room for the conversation beside the panel, so the
    /// panel has its place (set by the workspace while laying out). There
    /// is no "back beside the conversation" to offer then.
    pub(crate) cramped: bool,
    /// The search address the native browser tabs were last given (the
    /// setting may change while they are open).
    search: String,
}

impl gpui_kit::EventEmitter<PanelEvent> for WorkbenchPanel {}

impl WorkbenchPanel {
    pub fn new(store: Entity<AppStore>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this, _, cx| {
            this.forget_gone_threads(cx);
            cx.notify();
        })
        .detach();
        Self {
            store,
            threads: HashMap::new(),
            add_anchor: Rc::default(),
            tab_scroll: gpui_kit::ScrollHandle::new(),
            shown: None,
            maximized: false,
            cramped: false,
            search: String::new(),
        }
    }

    /// Let go of what was kept for threads that are gone for good (deleted:
    /// the store has dropped them). Each kept its file tree, its changes,
    /// its open files, its shells with their scrollback and its web pages
    /// (a WebKit process apiece, still polled) for as long as the app ran.
    fn forget_gone_threads(&mut self, cx: &mut Context<Self>) {
        let gone: Vec<ThreadId> = {
            let store = self.store.read(cx);
            self.threads
                .keys()
                .filter(|id| !store.threads.contains_key(*id))
                .cloned()
                .collect()
        };
        for id in gone {
            let Some(panel) = self.threads.remove(&id) else {
                continue;
            };
            // As closing each tab by hand would: the daemon ends the shells
            // and the pages.
            for tab in &panel.tabs {
                match tab {
                    Open::Terminal { pane, .. } => pane.update(cx, |p, cx| p.close(cx)),
                    Open::Page { view } => view.update(cx, |v, cx| v.close(cx)),
                    _ => {}
                }
            }
            for (_, pane) in &panel.dock {
                pane.update(cx, |p, cx| p.close(cx));
            }
            if self.shown.as_ref().is_some_and(|(shown, _)| *shown == id) {
                self.shown = None;
            }
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
                    dock: Vec::new(),
                    dock_active: 0,
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
            for (_, pane) in &panel.dock {
                pane.update(cx, |p, cx| p.restart(cx));
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
        // The native web view is the desktop app's; a browser tab, or a
        // window on another computer, has the streamed page instead
        // (`page`).
        if self.store.read(cx).is_elsewhere() {
            return None;
        }
        if let Some(view) = self.ensure_id(thread_id, window, cx)?.browser() {
            return Some(view);
        }
        Some(self.new_browser(thread_id, window, cx))
    }

    /// The selected thread's streamed page (the web client's browser tab),
    /// opened if it has none.
    pub fn page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<page::PageView>> {
        let (id, _) = self.selected(cx)?;
        self.ensure_id(&id, window, cx)?;
        let find = |this: &Self| {
            this.threads
                .get(&id)?
                .tabs
                .iter()
                .rev()
                .find_map(|tab| match tab {
                    Open::Page { view } => Some(view.clone()),
                    _ => None,
                })
        };
        if let Some(view) = find(self) {
            return Some(view);
        }
        self.add(&id, Tab::Browser, window, cx);
        find(self)
    }

    /// `thread_id`'s browser tab, if it has one.
    pub fn existing_browser(
        &self,
        thread_id: &ThreadId,
        _cx: &gpui_kit::App,
    ) -> Option<Entity<BrowserView>> {
        self.threads.get(thread_id)?.browser()
    }

    /// The preview of the document `path` open in `thread_id`'s panel.
    pub fn doc_preview(
        &self,
        thread_id: &ThreadId,
        path: &str,
        cx: &gpui_kit::App,
    ) -> Option<Entity<docs::DocPreview>> {
        self.threads
            .get(thread_id)?
            .tabs
            .iter()
            .find_map(|tab| match tab {
                Open::File { path: p, viewer } if p == path => viewer.read(cx).doc(),
                _ => None,
            })
    }

    fn new_browser(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserView> {
        let view = cx.new(|cx| BrowserView::new(window, cx));
        let search = self.store.read(cx).browser.search_template();
        view.update(cx, |b, _| b.set_search_template(search));
        let id = thread_id.clone();
        let subs = [cx.subscribe(&view, move |_, _, event, cx| match event {
            loft_browser::BrowserEvent::Annotation(note) => {
                cx.emit(PanelEvent::Annotation(id.clone(), note.clone()))
            }
            loft_browser::BrowserEvent::Screenshot(png) => {
                cx.emit(PanelEvent::Screenshot(id.clone(), png.clone()))
            }
            loft_browser::BrowserEvent::AnnotatingStopped => {
                cx.emit(PanelEvent::AnnotatingStopped(id.clone()))
            }
            // Titles, loading and sound show on the tab.
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

    /// A new shell for `thread_id`, in the side panel or the dock. The
    /// first one takes the thread's main key (reattaching its shell).
    fn new_terminal(
        &mut self,
        thread_id: &ThreadId,
        docked: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let base = format!("thread:{}", thread_id.as_str());
        let Some(panel) = self.threads.get(thread_id) else {
            return;
        };
        let key = next_terminal_key(&base, &panel.terminal_keys());
        let restore = !panel.terminals_restored;
        let pane = self.make_terminal(thread_id, key.clone(), window, cx);
        if let Some(panel) = self.threads.get_mut(thread_id) {
            if docked {
                panel.dock.push((key, pane.clone()));
                panel.dock_active = panel.dock.len() - 1;
            } else {
                panel.tabs.push(Open::Terminal {
                    key,
                    pane: pane.clone(),
                });
                panel.active = panel.tabs.len() - 1;
            }
        }
        if focus {
            focus_terminal(&pane, window, cx);
        }
        cx.notify();
        if restore {
            self.restore_terminals(thread_id, base, window, cx);
        }
    }

    fn make_terminal(
        &mut self,
        thread_id: &ThreadId,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalPane> {
        let store = self.store.clone();
        let root = FsRoot::Thread(thread_id.clone());
        let pane = cx.new(|cx| TerminalPane::new(store, root, key, window, cx));
        let sub = cx.observe(&pane, |_, _, cx| cx.notify());
        let id = thread_id.clone();
        let to_message = cx.subscribe(&pane, move |_, _, event: &terminal::AddToMessage, cx| {
            cx.emit(PanelEvent::AddToMessage(id.clone(), event.0.clone()))
        });
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel._subs.push(sub);
            panel._subs.push(to_message);
        }
        pane
    }

    fn push_terminal(
        &mut self,
        thread_id: &ThreadId,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.make_terminal(thread_id, key.clone(), window, cx);
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel.tabs.push(Open::Terminal {
                key,
                pane: pane.clone(),
            });
            panel.active = panel.tabs.len() - 1;
        }
        focus_terminal(&pane, window, cx);
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
                    let open = panel.terminal_keys();
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
            Tab::Terminal => self.new_terminal(thread_id, false, true, window, cx),
            // No native web view in a browser tab: a streamed page of the
            // daemon's own browser instead.
            // So does a window on another computer: this one's web view
            // can't reach that one's `localhost`.
            Tab::Browser if self.store.read(cx).is_elsewhere() => {
                let store = self.store.clone();
                let thread = thread_id.clone();
                let view = cx.new(|cx| page::PageView::new(store, thread, window, cx));
                // Titles and loading state show on the tab.
                let sub = cx.observe(&view, |_, _, cx| cx.notify());
                if let Some(panel) = self.threads.get_mut(thread_id) {
                    panel._subs.push(sub);
                    panel.tabs.push(Open::Page { view: view.clone() });
                    panel.active = panel.tabs.len() - 1;
                }
                // A blank tab waits for an address; one with a home page
                // shows where it went.
                view.update(cx, |v, cx| {
                    if v.opens_blank() {
                        v.focus_address_bar(window, cx);
                    }
                });
            }
            Tab::Browser => {
                let view = self.new_browser(thread_id, window, cx);
                // A new tab opens the home page, if one is set.
                let home = self.store.read(cx).browser.home().map(str::to_owned);
                view.update(cx, |b, cx| match home {
                    Some(home) => b.navigate(&home, cx),
                    None => b.focus_address_bar(window, cx),
                });
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
            focus_terminal(pane, window, cx);
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
        if let Open::Page { view } = &tab {
            view.update(cx, |v, cx| v.close(cx));
        }
        if panel.active > ix || panel.active >= panel.tabs.len() {
            panel.active = panel.active.saturating_sub(1);
        }
        cx.notify();
    }

    /// Show what one turn changed in the Changes tab (at `path`, if set).
    pub fn open_turn_diff(
        &mut self,
        thread_id: &ThreadId,
        turn_id: loft_protocol::TurnId,
        path: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.ensure_id(thread_id, window, cx) else {
            return;
        };
        panel
            .changes
            .update(cx, |c, cx| c.show_turn(turn_id, path, cx));
        self.add(thread_id, Tab::Changes, window, cx);
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
                let (thread, file) = (thread_id.clone(), rel_path.clone());
                cx.subscribe(&viewer, move |_, _, event: &viewer::ViewerEvent, cx| {
                    let viewer::ViewerEvent::Note(note) = event;
                    cx.emit(PanelEvent::DocNote(
                        thread.clone(),
                        file.clone(),
                        note.clone(),
                    ));
                })
                .detach();
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

    /// Close every tab of `thread_id` for which `pred(index)` holds.
    fn close_where(
        &mut self,
        thread_id: &ThreadId,
        pred: impl Fn(usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        let count = self.threads.get(thread_id).map_or(0, |p| p.tabs.len());
        for ix in (0..count).rev() {
            if pred(ix) {
                self.close(thread_id, ix, cx);
            }
        }
    }

    /// Right-click on a tab: close it, the others, those to its right, or all.
    fn tab_menu(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        at: gpui_kit::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use loft_browser::menu::{self, Entry};
        let count = self.threads.get(thread_id).map_or(0, |p| p.tabs.len());
        let panel = cx.weak_entity();
        let item = |label: &str, icon: IconName, pred: fn(usize, usize) -> bool| {
            let (panel, id) = (panel.clone(), thread_id.clone());
            Entry::item(label.to_owned(), icon, move |cx| {
                _ = panel.update(cx, |p, cx| p.close_where(&id, |i| pred(i, ix), cx));
            })
        };
        let mut entries = vec![
            item("Close", IconName::X, |i, ix| i == ix),
            item("Close Others", IconName::Minus, |i, ix| i != ix).disabled(count < 2),
            item("Close to the Right", IconName::ArrowRight, |i, ix| i > ix)
                .disabled(ix + 1 >= count),
            Entry::Separator,
            item("Close All", IconName::Trash, |_, _| true),
        ];
        // A shell can move under the conversation.
        if let Some(Open::Terminal { .. }) =
            self.threads.get(thread_id).and_then(|p| p.tabs.get(ix))
        {
            let (panel, id, handle) = (panel.clone(), thread_id.clone(), window.window_handle());
            entries.insert(
                0,
                Entry::item(
                    "Move to Bottom Panel".to_owned(),
                    IconName::PanelBottom,
                    move |cx| {
                        let (panel, id) = (panel.clone(), id.clone());
                        let _ = handle.update(cx, move |_, window, cx| {
                            _ = panel.update(cx, |p, cx| p.move_to_dock(&id, ix, window, cx));
                        });
                    },
                ),
            );
            entries.insert(1, Entry::Separator);
        }
        // A page tab also offers a reload.
        if let Some(Open::Browser { view }) =
            self.threads.get(thread_id).and_then(|p| p.tabs.get(ix))
        {
            let view = view.downgrade();
            entries.insert(
                0,
                Entry::item("Reload Page".to_owned(), IconName::RotateCw, move |cx| {
                    _ = view.update(cx, |b, cx| b.reload(cx));
                }),
            );
            entries.insert(1, Entry::Separator);
        }
        menu::open_at(window, at, entries, cx);
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

/// The first unused reattach key: the thread's main one, then `#2`, `#3`…
fn next_terminal_key(base: &str, used: &[String]) -> String {
    if !used.iter().any(|k| k == base) {
        return base.to_owned();
    }
    let next = used
        .iter()
        .filter_map(|k| k.rsplit_once('#').and_then(|(_, n)| n.parse::<u32>().ok()))
        .max()
        .unwrap_or(1)
        + 1;
    format!("{base}#{next}")
}

fn focus_terminal(pane: &Entity<TerminalPane>, window: &mut Window, cx: &mut gpui_kit::App) {
    let view = pane.read(cx).view.clone();
    window.focus(&gpui_kit::Focusable::focus_handle(&view, cx), cx);
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
        Open::Page { view } => {
            let v = view.read(cx);
            let title = v
                .title()
                .filter(|t| !t.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| v.url().and_then(host))
                .unwrap_or_else(|| "New Tab".into());
            let face = if v.is_loading() {
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
        // The search engine was changed in Settings: the native browser
        // tabs that are open search with it from now on.
        let search = self.store.read(cx).browser.search_template();
        if search != self.search {
            self.search = search;
            let views: Vec<Entity<BrowserView>> = self
                .threads
                .values()
                .flat_map(|panel| &panel.tabs)
                .filter_map(|tab| match tab {
                    Open::Browser { view } => Some(view.clone()),
                    _ => None,
                })
                .collect();
            for view in views {
                let search = self.search.clone();
                view.update(cx, |b, _| b.set_search_template(search));
            }
        }
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
            // A page making sound (or muted) shows a speaker that mutes it.
            let sound = match tab {
                Open::Browser { view } => {
                    let b = view.read(cx);
                    (b.is_playing_audio() || b.is_muted()).then(|| (view.clone(), b.is_muted()))
                }
                _ => None,
            };
            let (tid, cid, mid) = (thread_id.clone(), thread_id.clone(), thread_id.clone());
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
                    .when_some(sound, |d, (view, muted)| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("panel-tab-sound-{ix}")))
                                .flex_none()
                                .cursor_pointer()
                                .p(px(2.))
                                .rounded(px(4.))
                                .text_color(p.muted_foreground)
                                .hover(|d| d.bg(p.border).text_color(p.foreground))
                                .child(
                                    Icon::new(if muted {
                                        IconName::VolumeX
                                    } else {
                                        IconName::Volume2
                                    })
                                    .xsmall(),
                                )
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(if muted {
                                        "Unmute tab"
                                    } else {
                                        "Mute tab"
                                    })
                                    .build(window, cx)
                                })
                                .claims_press()
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    view.update(cx, |b, cx| b.set_muted(!muted, cx));
                                }),
                        )
                    })
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
                            .claims_press()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close(&cid, ix, cx);
                            })),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(&tid, ix, window, cx)),
                    )
                    .on_mouse_down(
                        gpui_kit::MouseButton::Right,
                        cx.listener(move |this, event: &gpui_kit::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.tab_menu(&mid, ix, event.position, window, cx);
                        }),
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
            // With no room beside the conversation there is nothing to
            // fill or go back to: the panel is all there is.
            .when(!self.cramped, |strip| {
                strip.child(
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
                )
            });

        let panel = self.threads.get(&thread_id).expect("ensured");
        let body: AnyElement = match panel.active_tab() {
            Some(Open::Files) => panel.files.clone().into_any_element(),
            Some(Open::Changes) => panel.changes.clone().into_any_element(),
            Some(Open::Terminal { pane, .. }) => pane.clone().into_any_element(),
            Some(Open::Browser { .. }) if crate::host::IS_WEB => {
                crate::host_ui::not_in_browser(IconName::Globe, "Browser", &p)
            }
            Some(Open::Browser { view }) => view.clone().into_any_element(),
            Some(Open::Page { view }) => view.clone().into_any_element(),
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
    use super::{host, next_terminal_key, terminal_title};

    #[test]
    fn terminal_keys_fill_in_order() {
        let keys = |v: &[&str]| v.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        assert_eq!(next_terminal_key("t", &[]), "t");
        assert_eq!(next_terminal_key("t", &keys(&["t"])), "t#2");
        assert_eq!(next_terminal_key("t", &keys(&["t", "t#2", "t#5"])), "t#6");
        // The main shell moved away (or closed): it is free again.
        assert_eq!(next_terminal_key("t", &keys(&["t#2"])), "t");
    }

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
