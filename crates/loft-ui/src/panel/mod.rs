//! The workbench panel beside the conversation: files, changes, terminal,
//! browser, open files and subagents — per thread.

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
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::{FsRoot, ItemId, ThreadId, TurnId};
use std::collections::HashMap;
use viewer::FileViewer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    Files,
    Changes,
    Terminal,
    Browser,
    File(String),
    Agent(ItemId),
}

struct ThreadPanel {
    files: Entity<FileTree>,
    changes: Entity<ChangesView>,
    viewers: Vec<(String, Entity<FileViewer>)>,
    agents: Vec<(ItemId, Entity<AgentView>)>,
    /// The thread's shell, created when the Terminal tab is first shown.
    terminal: Option<Entity<terminal::TerminalTabs>>,
    active: Tab,
    /// Last finished turn we refreshed for.
    seen_turn: Option<TurnId>,
    /// The thread's working directory; changes when its worktree is created.
    cwd: String,
    _subs: Vec<Subscription>,
}

pub struct WorkbenchPanel {
    store: Entity<AppStore>,
    threads: HashMap<ThreadId, ThreadPanel>,
    /// One shared browser (agents drive it; created on first use).
    browser: Option<Entity<loft_browser::BrowserView>>,
}

impl WorkbenchPanel {
    pub fn new(store: Entity<AppStore>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            threads: HashMap::new(),
            browser: None,
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
            let seen_turn = thread
                .read(cx)
                .model
                .as_ref()
                .and_then(|m| m.last_turn())
                .filter(|t| t.status.is_terminal())
                .map(|t| t.id.clone());
            let cwd = thread
                .read(cx)
                .model
                .as_ref()
                .map(|m| m.thread.cwd.clone())
                .unwrap_or_default();
            self.threads.insert(
                thread_id.clone(),
                ThreadPanel {
                    files,
                    changes,
                    viewers: Vec::new(),
                    agents: Vec::new(),
                    terminal: None,
                    active: Tab::Files,
                    seen_turn,
                    cwd,
                    _subs: subs,
                },
            );
        }
        self.threads.get_mut(thread_id).expect("inserted above")
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
        if moved && let Some(term) = &panel.terminal {
            term.update(cx, |t, cx| t.restart_all(cx));
        }
        if moved || (finished.is_some() && finished != panel.seen_turn) {
            if finished.is_some() {
                panel.seen_turn = finished;
            }
            panel.files.update(cx, |f, cx| f.refresh(cx));
            panel.changes.update(cx, |c, cx| c.refresh(cx));
            for (_, viewer) in &panel.viewers {
                viewer.update(cx, |v, cx| v.reload(window, cx));
            }
        }
    }

    /// The browser panel, created on first use.
    pub fn browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<loft_browser::BrowserView> {
        if let Some(b) = &self.browser {
            return b.clone();
        }
        let browser = cx.new(|cx| loft_browser::BrowserView::new(window, cx));
        self.browser = Some(browser.clone());
        browser
    }

    /// Show a tab for the selected thread (creating file/agent views).
    pub fn show(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, thread)) = self.selected(cx) else {
            return;
        };
        self.ensure(&id, &thread, window, cx).active = tab.clone();
        if tab == Tab::Terminal {
            let tabs = self.ensure_terminal(&id, window, cx);
            tabs.update(cx, |t, cx| t.focus(window, cx));
        }
        cx.notify();
    }

    /// The thread's terminal tabs (reattachable daemon shells).
    fn ensure_terminal(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<terminal::TerminalTabs> {
        let store = self.store.clone();
        let panel = self.threads.get_mut(thread_id).expect("ensured");
        panel
            .terminal
            .get_or_insert_with(|| {
                let root = FsRoot::Thread(thread_id.clone());
                let key = format!("thread:{}", thread_id.as_str());
                cx.new(|cx| terminal::TerminalTabs::new(store, root, key, window, cx))
            })
            .clone()
    }

    pub fn open_file(
        &mut self,
        thread_id: &ThreadId,
        rel_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned() else {
            return;
        };
        let store = self.store.clone();
        let panel = self.ensure(thread_id, &thread, window, cx);
        if !panel.viewers.iter().any(|(p, _)| *p == rel_path) {
            let root = FsRoot::Thread(thread_id.clone());
            let path = rel_path.clone();
            let viewer = cx.new(|cx| FileViewer::new(store, root, path, window, cx));
            panel.viewers.push((rel_path.clone(), viewer));
        }
        panel.active = Tab::File(rel_path);
        cx.notify();
    }

    pub fn open_agent(
        &mut self,
        thread_id: &ThreadId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.store.read(cx).threads.get(thread_id).cloned() else {
            return;
        };
        let store = self.store.clone();
        let panel = self.ensure(thread_id, &thread, window, cx);
        if !panel.agents.iter().any(|(i, _)| *i == item_id) {
            let (t, i) = (thread_id.clone(), item_id.clone());
            let view = cx.new(|cx| AgentView::new(store, t, i, cx));
            panel.agents.push((item_id.clone(), view));
        }
        panel.active = Tab::Agent(item_id);
        cx.notify();
    }

    fn close(&mut self, thread_id: &ThreadId, tab: &Tab, cx: &mut Context<Self>) {
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        match tab {
            Tab::File(path) => panel.viewers.retain(|(p, _)| p != path),
            Tab::Agent(id) => panel.agents.retain(|(i, _)| i != id),
            _ => return,
        }
        if panel.active == *tab {
            panel.active = Tab::Files;
        }
        cx.notify();
    }

    /// The browser, if it was ever opened.
    pub fn existing_browser(&self) -> Option<Entity<loft_browser::BrowserView>> {
        self.browser.clone()
    }

    pub fn active_tab(&self, cx: &gpui_kit::App) -> Option<Tab> {
        let id = self.store.read(cx).selected.clone()?;
        self.threads.get(&id).map(|p| p.active.clone())
    }
}

fn tab_chip(
    id: SharedString,
    icon: AnyElement,
    label: String,
    active: bool,
    closable: bool,
    p: &Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    h_flex()
        .id(id)
        .flex_shrink_0()
        .max_w(px(200.))
        .gap_1p5()
        .items_center()
        .pl_2()
        .pr(if closable { px(4.) } else { px(8.) })
        .h(px(28.))
        .rounded(px(7.))
        .text_xs()
        .cursor_pointer()
        .when(active, |d| {
            d.bg(p.selected)
                .text_color(p.foreground)
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!active, |d| {
            d.text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
        })
        .child(icon)
        .when(active || closable, |d| {
            d.child(div().truncate().child(label))
        })
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
        let store = self.store.clone();
        let panel = self.ensure(&thread_id, &thread, window, cx);
        let active = panel.active.clone();

        // Files, Changes, Terminal and Browser live in the title bar; the
        // strip only lists the files and agents opened here.
        let mut tabs: Vec<(Tab, AnyElement, String, bool)> = Vec::new();
        for (path, _) in &panel.viewers {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            tabs.push((
                Tab::File(path.clone()),
                crate::file_icons::icon(&name, false, false, px(14.)),
                name,
                true,
            ));
        }
        let agent_titles: Vec<(ItemId, String)> = panel
            .agents
            .iter()
            .map(|(id, view)| (id.clone(), view.read(cx).title(cx)))
            .collect();
        for (id, title) in agent_titles {
            tabs.push((
                Tab::Agent(id),
                Icon::new(IconName::Bot).xsmall().into_any_element(),
                title,
                true,
            ));
        }

        let has_tabs = !tabs.is_empty();
        let mut strip = h_flex()
            .id("panel-tabs")
            .h(px(40.))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(p.border);
        for (ix, (tab, icon, label, closable)) in tabs.into_iter().enumerate() {
            let is_active = tab == active;
            let chip_tab = tab.clone();
            let mut chip = tab_chip(
                SharedString::from(format!("panel-tab-{ix}")),
                icon,
                label,
                is_active,
                closable,
                &p,
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.show(chip_tab.clone(), window, cx)),
            );
            if closable {
                let close_tab = tab.clone();
                let tid = thread_id.clone();
                chip = chip.child(
                    div()
                        .id(SharedString::from(format!("panel-close-{ix}")))
                        .cursor_pointer()
                        .p(px(2.))
                        .rounded(px(4.))
                        .hover(|d| d.bg(p.border))
                        .child(Icon::new(IconName::X).xsmall())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close(&tid, &close_tab, cx)
                        })),
                );
            }
            strip = strip.child(chip);
        }

        if active == Tab::Terminal {
            self.ensure_terminal(&thread_id, window, cx);
        }
        let panel = self.threads.get(&thread_id).expect("ensured");
        let body: AnyElement = match &active {
            Tab::Files => panel.files.clone().into_any_element(),
            Tab::Changes => panel.changes.clone().into_any_element(),
            Tab::File(path) => panel
                .viewers
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, v)| v.clone().into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            Tab::Agent(id) => panel
                .agents
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, v)| v.clone().into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            Tab::Terminal => match &panel.terminal {
                Some(pane) => pane.clone().into_any_element(),
                None => placeholder(&p, IconName::SquareTerminal, "Terminal"),
            },
            Tab::Browser => self.browser(window, cx).into_any_element(),
        };
        let _ = store;
        v_flex()
            .size_full()
            .bg(p.background)
            .when(has_tabs, |d| d.child(strip))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

fn placeholder(p: &Palette, icon: IconName, label: &str) -> AnyElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .text_sm()
        .text_color(p.muted_foreground)
        .child(Icon::new(icon).large())
        .child(format!("{label} is starting…"))
        .into_any_element()
}
