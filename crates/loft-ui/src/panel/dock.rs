//! The terminal dock under the conversation (Ctrl+`), like an editor's
//! bottom panel. Its shells live in the workbench panel's per-thread state,
//! so one moves between the dock and the side panel without restarting.

use super::{Open, PanelEvent, WorkbenchPanel, focus_terminal, terminal_title};
use crate::press::ClaimsPress as _;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use loft_protocol::ThreadId;

impl WorkbenchPanel {
    /// The selected thread's docked shells: how many.
    pub fn dock_len(&self, cx: &gpui_kit::App) -> usize {
        self.store
            .read(cx)
            .selected
            .as_ref()
            .and_then(|id| self.threads.get(id))
            .map_or(0, |p| p.dock.len())
    }

    /// Give the selected thread a docked shell if it has none.
    pub fn ensure_docked(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, thread)) = self.selected(cx) else {
            return;
        };
        if self.ensure(&id, &thread, window, cx).dock.is_empty() {
            self.new_terminal(&id, true, focus, window, cx);
        } else if focus {
            self.focus_docked(window, cx);
        }
    }

    /// Focus the selected thread's showing docked shell.
    pub fn focus_docked(&self, window: &mut Window, cx: &mut gpui_kit::App) {
        if let Some(pane) = self.docked_pane(cx) {
            focus_terminal(&pane, window, cx);
        }
    }

    /// Whether the selected thread's showing docked shell has focus.
    pub fn docked_focused(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        self.docked_pane(cx).is_some_and(|pane| {
            let view = pane.read(cx).view.clone();
            gpui_kit::Focusable::focus_handle(&view, cx).contains_focused(window, cx)
        })
    }

    fn docked_pane(&self, cx: &gpui_kit::App) -> Option<Entity<super::TerminalPane>> {
        let id = self.store.read(cx).selected.clone()?;
        let panel = self.threads.get(&id)?;
        panel.dock.get(panel.dock_active).map(|(_, p)| p.clone())
    }

    fn activate_docked(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        if let Some((_, pane)) = panel.dock.get(ix) {
            panel.dock_active = ix;
            focus_terminal(pane, window, cx);
            cx.notify();
        }
    }

    /// Take a docked shell out of the dock (still running).
    fn take_docked(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<(String, Entity<super::TerminalPane>)> {
        let panel = self.threads.get_mut(thread_id)?;
        if ix >= panel.dock.len() {
            return None;
        }
        let taken = panel.dock.remove(ix);
        if panel.dock_active > ix || panel.dock_active >= panel.dock.len() {
            panel.dock_active = panel.dock_active.saturating_sub(1);
        }
        if panel.dock.is_empty() {
            cx.emit(PanelEvent::HideDock);
        }
        cx.notify();
        Some(taken)
    }

    fn close_docked(&mut self, thread_id: &ThreadId, ix: usize, cx: &mut Context<Self>) {
        if let Some((_, pane)) = self.take_docked(thread_id, ix, cx) {
            pane.update(cx, |p, cx| p.close(cx));
        }
    }

    /// Send a docked shell to the side panel, as its showing tab.
    fn move_to_panel(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((key, pane)) = self.take_docked(thread_id, ix, cx) else {
            return;
        };
        if let Some(panel) = self.threads.get_mut(thread_id) {
            panel.tabs.push(Open::Terminal {
                key,
                pane: pane.clone(),
            });
            panel.active = panel.tabs.len() - 1;
        }
        focus_terminal(&pane, window, cx);
        cx.emit(PanelEvent::ShowPanel);
        cx.notify();
    }

    /// Send a side-panel shell tab to the dock, as its showing shell.
    pub(super) fn move_to_dock(
        &mut self,
        thread_id: &ThreadId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.threads.get_mut(thread_id) else {
            return;
        };
        if !matches!(panel.tabs.get(ix), Some(Open::Terminal { .. })) {
            return;
        }
        let Open::Terminal { key, pane } = panel.tabs.remove(ix) else {
            return;
        };
        if panel.active > ix || panel.active >= panel.tabs.len() {
            panel.active = panel.active.saturating_sub(1);
        }
        panel.dock.push((key, pane.clone()));
        panel.dock_active = panel.dock.len() - 1;
        focus_terminal(&pane, window, cx);
        cx.emit(PanelEvent::ShowDock);
        cx.notify();
    }

    /// The dock: its shells' tabs, then the showing one.
    fn render_dock(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::of(cx);
        let Some(thread_id) = self.store.read(cx).selected.clone() else {
            return div().size_full().bg(p.background).into_any_element();
        };
        let (dock, active) = match self.threads.get(&thread_id) {
            Some(panel) => (panel.dock.clone(), panel.dock_active),
            None => (Vec::new(), 0),
        };

        let mut tabs = h_flex()
            .id("dock-tabs")
            .flex_shrink(1.)
            .min_w_0()
            .h_full()
            .gap_1()
            .items_center()
            .overflow_x_scroll();
        for (ix, (_, pane)) in dock.iter().enumerate() {
            let is_active = ix == active;
            let label = terminal_title(&pane.read(cx).label());
            let (tid, cid) = (thread_id.clone(), thread_id.clone());
            tabs = tabs.child(
                h_flex()
                    .id(SharedString::from(format!("dock-tab-{ix}")))
                    .group("dock-tab")
                    .flex_shrink(1.)
                    .w(px(160.))
                    .min_w(px(72.))
                    .h(px(24.))
                    .pl_2()
                    .pr_1()
                    .gap_1p5()
                    .items_center()
                    .rounded(px(6.))
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
                    .child(Icon::new(IconName::SquareTerminal).xsmall())
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .id(SharedString::from(format!("dock-tab-x-{ix}")))
                            .flex_none()
                            .p(px(2.))
                            .rounded(px(4.))
                            .when(!is_active, |d| {
                                d.invisible().group_hover("dock-tab", |d| d.visible())
                            })
                            .hover(|d| d.bg(p.border))
                            .child(Icon::new(IconName::X).xsmall())
                            .claims_press()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_docked(&cid, ix, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_docked(&tid, ix, window, cx)
                    })),
            );
        }

        let button = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .flex_none()
                .p(px(4.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_color(p.muted_foreground)
                .hover(|d| d.bg(p.hover).text_color(p.foreground))
                .child(Icon::new(icon).small())
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                })
        };
        let (new_id, move_id) = (thread_id.clone(), thread_id.clone());
        let strip = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .px_1p5()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .bg(p.sidebar)
            .child(tabs)
            .child(
                button("dock-new", IconName::Plus, "New terminal").on_click(cx.listener(
                    move |this, _, window, cx| this.new_terminal(&new_id, true, true, window, cx),
                )),
            )
            .child(div().flex_1())
            .when(!dock.is_empty(), |d| {
                d.child(
                    button("dock-to-panel", IconName::PanelRight, "Move to side panel").on_click(
                        cx.listener(move |this, _, window, cx| {
                            let ix = this.threads.get(&move_id).map_or(0, |p| p.dock_active);
                            this.move_to_panel(&move_id, ix, window, cx)
                        }),
                    ),
                )
            })
            .child(
                button("dock-hide", IconName::ChevronDown, "Hide (Ctrl+`)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::HideDock))),
            );

        let body = match dock.get(active) {
            Some((_, pane)) => pane.clone().into_any_element(),
            None => div().into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(p.background)
            .child(strip)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

/// The dock's view: drawn from the workbench panel, which owns the shells.
pub struct TerminalDock {
    panel: Entity<WorkbenchPanel>,
    _sub: Subscription,
}

impl TerminalDock {
    pub fn new(panel: Entity<WorkbenchPanel>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&panel, |_, _, cx| cx.notify());
        Self { panel, _sub: sub }
    }
}

impl Render for TerminalDock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.panel.update(cx, |panel, cx| panel.render_dock(cx))
    }
}
