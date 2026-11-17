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
    /// The project action this shell runs (its key says): the tab's name.
    action: Option<String>,
    /// A command to type into the shell the next time one is opened.
    run: Option<String>,
    error: Option<String>,
    /// Last shown without the right to a terminal (a browser session with
    /// terminals off); opens its shell once that changes.
    blocked: bool,
    _subs: Vec<Subscription>,
}

/// The terminal's selection, asked to go into the thread's message
/// ("Add Selection to Message").
pub struct AddToMessage(pub String);

impl gpui_kit::EventEmitter<AddToMessage> for TerminalPane {}

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
        let action = loft_protocol::action_of_terminal_key(&key).map(str::to_owned);
        Self {
            store,
            root,
            action,
            run: None,
            key,
            view,
            id: None,
            opening: false,
            size: (0, 0),
            exited: None,
            title: None,
            error: None,
            blocked: false,
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
            // For the composer of the thread this shell belongs to.
            TerminalEvent::AddToMessage(text) => cx.emit(AddToMessage(text.clone())),
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
            command: self.run.take(),
        };
        request_in(&self.store, body, cx, |this, result, cx| {
            this.opening = false;
            match result {
                Ok(Response::Terminal { terminal }) => this.attach(terminal, cx),
                Ok(other) => this.error = Some(format!("unexpected response: {other:?}")),
                Err(err) => this.error = Some(err.message),
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The daemon's shell for this pane: follow it.
    fn attach(&mut self, terminal: loft_protocol::TerminalInfo, cx: &mut Context<Self>) {
        self.id = Some(terminal.id.clone());
        self.exited = None;
        // The grid may have changed while the open was in flight.
        let (cols, rows) = self.size;
        if cols > 0 && (cols, rows) != (terminal.cols, terminal.rows) {
            let resize = Request::TerminalResize {
                id: terminal.id.clone(),
                cols,
                rows,
            };
            self.send(resize, cx);
        }
        let weak = cx.entity().downgrade();
        self.store
            .update(cx, |s, cx| s.subscribe_terminal(terminal.id, weak, cx));
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

    /// Tab label: the action it runs, else the program's title, else the
    /// shell's name.
    pub fn label(&self) -> String {
        match (&self.action, &self.title) {
            (Some(action), _) => action.clone(),
            (None, Some(t)) if !t.is_empty() => t.clone(),
            _ => "Shell".into(),
        }
    }

    /// Run `command` here, typed as the user would: in the shell about to
    /// open, in a new one if the last has exited, and in a live one only
    /// if what it ran before has ended (the daemon can tell).
    pub fn run(&mut self, command: String, cx: &mut Context<Self>) {
        if self.exited.is_some() {
            self.run = Some(command);
            self.restart(cx);
        } else if self.id.is_some() {
            // The same key: the daemon answers with the shell it has (no
            // size given, so that is left alone).
            let body = Request::TerminalOpen {
                root: self.root.clone(),
                cols: 0,
                rows: 0,
                key: Some(self.key.clone()),
                command: Some(command),
            };
            request_in(&self.store, body, cx, |this, result, cx| {
                // The shell had just ended, so this is a new one.
                if let Ok(Response::Terminal { terminal }) = result
                    && this.id.as_ref().is_some_and(|id| *id != terminal.id)
                {
                    if let Some(old) = this.id.take() {
                        this.store.update(cx, |s, _| s.forget_terminal(&old));
                    }
                    this.view.update(cx, |v, cx| v.reset(cx));
                    this.attach(terminal, cx);
                    cx.notify();
                }
            });
        } else if !self.opening {
            // Opens once the view knows its size.
            self.run = Some(command);
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
            let _ = this.update(cx, |this, cx| {
                if this.store.read(cx).terminals_allowed() {
                    this.open(cx);
                } else {
                    // Not ours to open right now; `render` opens it once
                    // terminals are allowed again.
                    this.opening = false;
                    this.blocked = true;
                }
            });
        })
        .detach();
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::of(cx);
        // A browser session the daemon doesn't give a shell to.
        if !self.store.read(cx).terminals_allowed() {
            self.blocked = true;
            return crate::host_ui::unavailable(
                IconName::SquareTerminal,
                "Terminals are off for browsers",
                "Turn them on in the desktop app: Settings ▸ Web access.",
                &p,
            );
        }
        if std::mem::take(&mut self.blocked) && self.id.is_none() && !self.opening {
            // Allowed again, and the shell it had is gone: open one.
            self.error = None;
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.id.is_none() && !this.opening {
                        this.open(cx);
                    }
                });
            })
            .detach();
        }
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
            .into_any_element()
    }
}
