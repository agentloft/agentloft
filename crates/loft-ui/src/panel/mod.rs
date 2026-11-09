//! The workbench panel beside the conversation: files, changes, terminal,
//! browser, open files and subagents — per thread.

pub mod agent;
pub mod changes;
pub mod docs;
pub mod files;
pub mod viewer;

use crate::store::{AppStore, ThreadState};
use crate::theme::Palette;
use agent::AgentView;
use changes::ChangesView;
use files::{FileTree, FileTreeEvent, file_icon};
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
    active: Tab,
    /// Last finished turn we refreshed for.
    seen_turn: Option<TurnId>,
    _subs: Vec<Subscription>,
}

pub struct WorkbenchPanel {
    store: Entity<AppStore>,
    threads: HashMap<ThreadId, ThreadPanel>,
    /// Extra tab contents provided by the host (terminal, browser).
    pub terminal: Option<AnyViewFn>,
    pub browser: Option<AnyViewFn>,
}

/// Renders a host-provided view for the selected thread.
pub type AnyViewFn = Box<dyn Fn(&ThreadId, &mut Window, &mut gpui_kit::App) -> AnyElement>;

impl WorkbenchPanel {
    pub fn new(store: Entity<AppStore>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            threads: HashMap::new(),
            terminal: None,
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
            self.threads.insert(
                thread_id.clone(),
                ThreadPanel {
                    files,
                    changes,
                    viewers: Vec::new(),
                    agents: Vec::new(),
                    active: Tab::Files,
                    seen_turn,
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
        let finished = thread
            .read(cx)
            .model
            .as_ref()
            .and_then(|m| m.last_turn())
            .filter(|t| t.status.is_terminal())
            .map(|t| t.id.clone());
        let Some(panel) = self.threads.get_mut(id) else {
            return;
        };
        if finished.is_some() && finished != panel.seen_turn {
            panel.seen_turn = finished;
            panel.files.update(cx, |f, cx| f.refresh(cx));
            panel.changes.update(cx, |c, cx| c.refresh(cx));
            for (_, viewer) in &panel.viewers {
                viewer.update(cx, |v, cx| v.reload(window, cx));
            }
        }
    }

    /// Show a tab for the selected thread (creating file/agent views).
    pub fn show(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, thread)) = self.selected(cx) else {
            return;
        };
        self.ensure(&id, &thread, window, cx).active = tab;
        cx.notify();
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

    pub fn active_tab(&self, cx: &gpui_kit::App) -> Option<Tab> {
        let id = self.store.read(cx).selected.clone()?;
        self.threads.get(&id).map(|p| p.active.clone())
    }
}

fn tab_chip(
    id: SharedString,
    icon: IconName,
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
        .child(Icon::new(icon).xsmall())
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

        // Fixed tabs, then open files and agents.
        let mut tabs: Vec<(Tab, IconName, String, bool)> = vec![
            (Tab::Files, IconName::FolderTree, "Files".into(), false),
            (Tab::Changes, IconName::GitCompare, "Changes".into(), false),
            (
                Tab::Terminal,
                IconName::SquareTerminal,
                "Terminal".into(),
                false,
            ),
            (Tab::Browser, IconName::Globe, "Browser".into(), false),
        ];
        for (path, _) in &panel.viewers {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            tabs.push((
                Tab::File(path.clone()),
                file_icon(&name, false, false),
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
            tabs.push((Tab::Agent(id), IconName::Bot, title, true));
        }

        let mut strip = h_flex()
            .id("panel-tabs")
            .h(px(44.))
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
            Tab::Terminal => match &self.terminal {
                Some(render) => render(&thread_id, window, cx),
                None => placeholder(&p, IconName::SquareTerminal, "Terminal"),
            },
            Tab::Browser => match &self.browser {
                Some(render) => render(&thread_id, window, cx),
                None => placeholder(&p, IconName::Globe, "Browser"),
            },
        };
        let _ = store;
        v_flex()
            .size_full()
            .bg(p.background)
            .child(strip)
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
