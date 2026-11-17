//! A browser tab the thread's agent handed to the person (ADR-0017 §9):
//! when the thread is open here, the panel comes up on that tab, once per
//! handoff, so its card (`panel/page/handoff.rs`) is in front of them. This
//! is the agent asking for the person, not an agent's tab taking the
//! window: its other tabs never switch what the panel shows.

use super::WorkspaceView;
use gpui_kit::{Context, Window};
use loft_protocol::{PageId, PageInfo, Request, Response, ThreadId};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Seen {
    /// Handoffs brought up already: the tab, and when it was handed over.
    shown: HashSet<(PageId, i64)>,
    /// The selected thread's handoff whose tabs were asked for last.
    asked: Option<(ThreadId, String)>,
}

impl WorkspaceView {
    /// The thread's tabs are `pages`: bring up one handed to the person, if
    /// it is new and the thread is the one open here.
    pub(super) fn show_handoff(
        &mut self,
        thread_id: &ThreadId,
        pages: &[PageInfo],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.store.read(cx).selected.as_ref() != Some(thread_id) {
            return;
        }
        let Some((page, since)) = pages
            .iter()
            .find_map(|p| Some((p, p.agent.handoff.as_ref()?.since_ms)))
        else {
            return;
        };
        if !self.handoffs.shown.insert((page.id.clone(), since)) {
            return;
        }
        self.panel_open = true;
        self.browser_preview = false;
        let page = page.clone();
        let view = self.panel.update(cx, |p, cx| {
            let view = p.tab_of(thread_id, page.clone(), window, cx)?;
            p.show_page(thread_id, &page.id, window, cx);
            Some(view)
        });
        if let Some(view) = view {
            view.update(cx, |v, cx| v.watch(cx));
        }
        cx.notify();
    }

    /// The thread open here waits for the person in one of its browser
    /// tabs (it says so before this window has heard of its tabs): ask for
    /// them, to bring that one up.
    pub(super) fn look_for_handoff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(selected) = store.selected.clone() else {
            return;
        };
        let reason = store
            .workspace
            .threads
            .iter()
            .find(|t| t.id == selected)
            .and_then(|t| t.handover.clone());
        let Some(reason) = reason else {
            self.handoffs.asked = None;
            return;
        };
        let asking = Some((selected.clone(), reason));
        // Asked once it can be: connected, and the browser allowed.
        let client = store.client().filter(|_| store.pages_allowed());
        let Some(client) = client.filter(|_| self.handoffs.asked != asking) else {
            return;
        };
        self.handoffs.asked = asking;
        let asked = client.request(Request::PageList {
            thread_id: selected.clone(),
        });
        cx.spawn_in(window, async move |this, cx| {
            let answer = asked.await;
            if !matches!(answer, Ok(Response::Pages { .. })) {
                // Ask again on the next change (the connection was new).
                let _ = this.update(cx, |this, _| this.handoffs.asked = None);
            }
            if let Ok(Response::Pages { pages }) = answer {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.panel.update(cx, |p, cx| {
                        p.sync_tabs(&selected, pages.clone(), window, cx)
                    });
                    this.show_handoff(&selected, &pages, window, cx);
                });
            }
        })
        .detach();
    }
}
