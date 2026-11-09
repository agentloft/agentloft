//! Terminal pane: a Ghostty-backed `TerminalView` (loft-terminal) wired to a
//! daemon-owned PTY (ADR-0007). Output streams in from the daemon (with the
//! scrollback replayed on (re)subscribe); keystrokes and resizes go back.

use crate::store::{AppStore, request_in};
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use loft_protocol::{FsRoot, Request, Response, TerminalId};
use loft_terminal::{TerminalEvent, TerminalView};

pub struct TerminalPane {
    store: Entity<AppStore>,
    root: FsRoot,
    /// Daemon reattach key (one main shell per thread).
    key: String,
    pub view: Entity<TerminalView>,
    pub id: Option<TerminalId>,
    opening: bool,
    size: (u16, u16),
    exited: Option<i32>,
    title: Option<String>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl TerminalPane {
    pub fn new(
        store: Entity<AppStore>,
        root: FsRoot,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| TerminalView::new(window, cx));
        let sub = cx.subscribe(&view, |this, _, event, cx| this.on_view_event(event, cx));
        Self {
            store,
            root,
            key,
            view,
            id: None,
            opening: false,
            size: (0, 0),
            exited: None,
            title: None,
            error: None,
            _subs: vec![sub],
        }
    }

    fn on_view_event(&mut self, event: &TerminalEvent, cx: &mut Context<Self>) {
        match event {
            TerminalEvent::Input(bytes) => {
                if self.exited.is_some() {
                    // "Press Enter to restart" after the shell exits.
                    if bytes.as_slice() == b"\r" {
                        self.restart(cx);
                    }
                } else if let Some(id) = &self.id {
                    self.send(
                        Request::TerminalInput {
                            id: id.clone(),
                            data: bytes.clone().into(),
                        },
                        cx,
                    );
                }
            }
            TerminalEvent::Resize { cols, rows, .. } => {
                self.size = (*cols, *rows);
                match &self.id {
                    Some(id) => self.send(
                        Request::TerminalResize {
                            id: id.clone(),
                            cols: *cols,
                            rows: *rows,
                        },
                        cx,
                    ),
                    // Open once we know the real size.
                    None if !self.opening => self.open(cx),
                    None => {}
                }
            }
            TerminalEvent::TitleChanged(title) => {
                self.title = Some(title.clone());
                cx.notify();
            }
            TerminalEvent::Bell => {}
            TerminalEvent::OpenUrl(url) => {
                // Local dev servers open next to the chat; the rest in the
                // default browser.
                let local = [
                    "http://localhost",
                    "http://127.0.0.1",
                    "http://0.0.0.0",
                    "https://localhost",
                ]
                .iter()
                .any(|p| url.starts_with(p));
                if local {
                    let url = url.clone();
                    self.store.update(cx, |_, cx| {
                        cx.emit(crate::store::StoreEvent::Browse { url })
                    });
                } else {
                    cx.open_url(url);
                }
            }
        }
    }

    /// Fire-and-forget request; the daemon answers keystrokes in order.
    fn send(&self, body: Request, cx: &mut Context<Self>) {
        if let Some(client) = self.store.read(cx).client() {
            let fut = client.request(body);
            cx.background_executor()
                .spawn(async move {
                    let _ = fut.await;
                })
                .detach();
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        self.opening = true;
        self.error = None;
        let (cols, rows) = if self.size.0 > 0 {
            self.size
        } else {
            (100, 30)
        };
        let body = Request::TerminalOpen {
            root: self.root.clone(),
            cols,
            rows,
            key: Some(self.key.clone()),
        };
        request_in(&self.store, body, cx, |this, result, cx| {
            this.opening = false;
            match result {
                Ok(Response::Terminal { terminal }) => {
                    this.id = Some(terminal.id.clone());
                    this.exited = None;
                    // The grid may have changed while the open was in flight.
                    let (cols, rows) = this.size;
                    if cols > 0 && (cols, rows) != (terminal.cols, terminal.rows) {
                        let resize = Request::TerminalResize {
                            id: terminal.id.clone(),
                            cols,
                            rows,
                        };
                        this.send(resize, cx);
                    }
                    let weak = cx.entity().downgrade();
                    this.store
                        .update(cx, |s, cx| s.subscribe_terminal(terminal.id, weak, cx));
                }
                Ok(other) => this.error = Some(format!("unexpected response: {other:?}")),
                Err(err) => this.error = Some(err.message),
            }
            cx.notify();
        });
        cx.notify();
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if self.opening {
            return;
        }
        self.exited = None;
        self.title = None;
        self.view.update(cx, |v, cx| v.reset(cx));
        match self.id.take() {
            // Close first: an open racing ahead would reattach the old shell.
            Some(id) => {
                self.opening = true;
                self.store.update(cx, |s, _| s.forget_terminal(&id));
                request_in(
                    &self.store,
                    Request::TerminalClose { id },
                    cx,
                    |this, _, cx| this.open(cx),
                );
            }
            None => self.open(cx),
        }
    }

    /// Tab label: the program's title, else the shell's name.
    pub fn label(&self) -> String {
        match &self.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => "Shell".into(),
        }
    }

    /// Close the shell for good (its tab is going away).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.id.take() {
            self.store.update(cx, |s, _| s.forget_terminal(&id));
            self.send(Request::TerminalClose { id }, cx);
        }
    }

    // -- fed by the store ---------------------------------------------------

    pub fn on_replay(&mut self, bytes: &[u8], exited: Option<i32>, cx: &mut Context<Self>) {
        self.view.update(cx, |v, cx| {
            v.reset(cx);
            v.feed(bytes, cx);
        });
        self.exited = exited;
        cx.notify();
    }

    pub fn on_output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.view.update(cx, |v, cx| v.feed(bytes, cx));
    }

    pub fn on_exit(&mut self, code: Option<i32>, cx: &mut Context<Self>) {
        self.exited = Some(code.unwrap_or(-1));
        cx.notify();
    }

    /// The daemon lost the terminal (e.g. it restarted): start a new one.
    /// Called while the store is updating, so the reopen (which reads the
    /// store) runs on the next tick.
    pub fn on_lost(&mut self, cx: &mut Context<Self>) {
        self.id = None;
        self.exited = None;
        self.opening = true;
        self.view.update(cx, |v, cx| v.reset(cx));
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| this.open(cx));
        })
        .detach();
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        let (mono, size) = (p.mono.clone(), p.mono_size);
        self.view.update(cx, |v, cx| v.set_font(mono, size, cx));
        let label = self.title.clone().unwrap_or_else(|| "Terminal".into());
        let header = h_flex()
            .h(px(32.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.border)
            .text_xs()
            .text_color(p.muted_foreground)
            .child(Icon::new(IconName::SquareTerminal).xsmall())
            .child(div().flex_1().min_w_0().truncate().child(label))
            .when_some(self.exited, |d, code| {
                d.child(
                    div()
                        .text_color(if code == 0 {
                            p.muted_foreground
                        } else {
                            p.warning
                        })
                        .child(format!("exited ({code}) · Enter to restart")),
                )
            })
            .child(
                h_flex()
                    .id("terminal-restart")
                    .gap_1()
                    .px_2()
                    .py(px(2.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover).text_color(p.foreground))
                    .child(Icon::new(IconName::RotateCw).xsmall())
                    .child("Restart")
                    .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
            );
        v_flex()
            .size_full()
            .child(header)
            .when_some(self.error.clone(), |d, err| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .bg(p.danger_soft)
                        .text_color(p.danger)
                        .child(err),
                )
            })
            .child(div().flex_1().min_h_0().child(self.view.clone()))
    }
}
