//! UI-side state: the daemon connection, the mirrored workspace, and one
//! [`ThreadState`] entity per open thread (ADR-0010: client I/O on tokio,
//! state applied on the GPUI main thread).

use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::component::text::TextViewState;
use gpui_kit::{App, AppContext as _, AsyncApp, Context, Entity, WeakEntity};
use loft_client::model::{ThreadModel, WorkspaceModel};
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_protocol::{
    ClientKind, DecisionKind, Delta, ErrorBody, Event, HarnessId, ItemBody, ItemId, ProjectId,
    Request, RequestId, Response, Snapshot, ThreadId, Topic,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

pub const BUILD: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected(String),
}

pub struct AppStore {
    runtime: tokio::runtime::Handle,
    paths: Paths,
    client: Option<Client>,
    pub conn: ConnState,
    pub workspace: WorkspaceModel,
    ws_sub: Option<u64>,
    /// Open thread views (subscribed).
    pub threads: HashMap<ThreadId, Entity<ThreadState>>,
    subs: HashMap<u64, ThreadId>,
    pub selected: Option<ThreadId>,
    /// Last request error, shown in the UI.
    pub last_error: Option<String>,
}

impl AppStore {
    pub fn new(runtime: tokio::runtime::Handle, paths: Paths, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            runtime,
            paths,
            client: None,
            conn: ConnState::Connecting,
            workspace: WorkspaceModel::default(),
            ws_sub: None,
            threads: HashMap::new(),
            subs: HashMap::new(),
            selected: None,
            last_error: None,
        };
        this.connect(cx);
        this
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.conn = ConnState::Connecting;
        cx.notify();
        let paths = self.paths.clone();
        let task = self.runtime.spawn(async move {
            loft_client::connect_or_spawn(
                &paths,
                &loft_client::daemon_binary(),
                ClientKind::LocalUi,
                BUILD,
            )
            .await
        });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = match task.await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!(e)),
            };
            match result {
                Ok((client, rx)) => {
                    let ok = this
                        .update(cx, |store, cx| store.on_connected(client, cx))
                        .is_ok();
                    if ok {
                        pump(this, rx, cx).await;
                    }
                }
                Err(err) => {
                    tracing::error!("connect failed: {err:#}");
                    let _ = this.update(cx, |store, cx| {
                        store.conn = ConnState::Disconnected(format!("{err:#}"));
                        cx.notify();
                    });
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let _ = this.update(cx, |store, cx| store.connect(cx));
                }
            }
        })
        .detach();
    }

    fn on_connected(&mut self, client: Client, cx: &mut Context<Self>) {
        tracing::info!(daemon = %client.welcome().daemon_build, pid = client.welcome().daemon_pid, "connected");
        self.conn = ConnState::Connected;
        self.subs.clear();
        self.ws_sub = Some(client.subscribe(Topic::Workspace));
        for id in self.threads.keys() {
            let sub = client.subscribe(Topic::Thread(id.clone()));
            self.subs.insert(sub, id.clone());
        }
        self.client = Some(client);
        cx.notify();
    }

    fn on_disconnected(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.conn = ConnState::Disconnected("connection to agentloftd lost".into());
        cx.notify();
        // Reconnect (respawning the daemon if it died).
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |store, cx| store.connect(cx));
        })
        .detach();
    }

    fn on_client_event(&mut self, event: ClientEvent, cx: &mut Context<Self>) {
        match event {
            ClientEvent::Snapshot { sub_id, snapshot } => {
                if Some(sub_id) == self.ws_sub {
                    self.workspace.apply_snapshot(snapshot);
                    // Reopen the most recently active thread on launch.
                    if self.selected.is_none()
                        && let Some(first) = self.workspace.threads.first()
                    {
                        let id = first.id.clone();
                        self.select_thread(id, cx);
                    }
                    cx.notify();
                } else if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_snapshot(snapshot, cx));
                }
            }
            ClientEvent::Event { sub_id, event } => {
                if Some(sub_id) == self.ws_sub {
                    if self.workspace.apply_event(&event) {
                        cx.notify();
                    }
                } else if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_event(&event, cx));
                }
            }
            ClientEvent::Delta { sub_id, delta } => {
                if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_delta(&delta, cx));
                }
            }
            ClientEvent::Fatal(err) => {
                self.last_error = Some(err.message);
                cx.notify();
            }
            ClientEvent::Disconnected => self.on_disconnected(cx),
        }
    }

    // -- actions ------------------------------------------------------------

    pub fn select_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        if !self.threads.contains_key(&id) {
            let entity = cx.new(|cx| ThreadState::new(id.clone(), cx));
            self.threads.insert(id.clone(), entity);
            if let Some(client) = &self.client {
                let sub = client.subscribe(Topic::Thread(id.clone()));
                self.subs.insert(sub, id.clone());
            }
        }
        self.selected = Some(id);
        cx.notify();
    }

    pub fn selected_thread(&self) -> Option<Entity<ThreadState>> {
        self.selected
            .as_ref()
            .and_then(|id| self.threads.get(id).cloned())
    }

    /// Fire a request; errors are surfaced in `last_error`.
    pub fn request(
        &mut self,
        body: Request,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Response, &mut Context<Self>) + 'static,
    ) {
        let Some(client) = self.client.clone() else {
            self.last_error = Some("not connected to agentloftd".into());
            cx.notify();
            return;
        };
        self.last_error = None;
        let fut = client.request(body);
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result: Result<Response, ErrorBody> = fut.await;
            let _ = this.update(cx, |store, cx| match result {
                Ok(resp) => then(store, resp, cx),
                Err(err) => {
                    store.last_error = Some(err.message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn add_project(&mut self, path: String, cx: &mut Context<Self>) {
        self.request(Request::AddProject { path, name: None }, cx, |_, _, _| {});
    }

    pub fn new_thread(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        self.request(
            Request::CreateThread {
                project_id,
                harness: HarnessId::claude(),
                title: None,
            },
            cx,
            |store, resp, cx| {
                if let Response::Thread { thread } = resp {
                    store.select_thread(thread.id, cx);
                }
            },
        );
    }

    pub fn send_message(&mut self, thread_id: ThreadId, text: String, cx: &mut Context<Self>) {
        self.request(Request::SendMessage { thread_id, text }, cx, |_, _, _| {});
    }

    pub fn interrupt(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        self.request(Request::Interrupt { thread_id }, cx, |_, _, _| {});
    }

    pub fn respond(
        &mut self,
        thread_id: ThreadId,
        request_id: RequestId,
        decision: DecisionKind,
        cx: &mut Context<Self>,
    ) {
        self.request(
            Request::RespondRequest {
                thread_id,
                request_id,
                decision,
            },
            cx,
            |_, _, _| {},
        );
    }
}

/// Drain client events into the store until the connection ends.
async fn pump(
    this: WeakEntity<AppStore>,
    mut rx: UnboundedReceiver<ClientEvent>,
    cx: &mut AsyncApp,
) {
    while let Some(event) = rx.recv().await {
        let disconnected = matches!(event, ClientEvent::Disconnected);
        if this
            .update(cx, |store, cx| store.on_client_event(event, cx))
            .is_err()
        {
            return;
        }
        if disconnected {
            return;
        }
    }
}

/// One open thread: its mirrored model plus view-side state (scroll list,
/// markdown documents for assistant messages).
pub struct ThreadState {
    pub id: ThreadId,
    pub model: Option<ThreadModel>,
    pub scroller: Entity<MessageScrollerState>,
    pub markdown: HashMap<ItemId, Entity<TextViewState>>,
}

impl ThreadState {
    fn new(id: ThreadId, cx: &mut Context<Self>) -> Self {
        Self {
            id,
            model: None,
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            markdown: HashMap::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.model.as_ref().map(|m| m.items.len()).unwrap_or(0)
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        let Some(model) = ThreadModel::from_snapshot(snapshot) else {
            return;
        };
        self.markdown.clear();
        for item in &model.items {
            self.sync_markdown(&item.id, &item.body, cx);
        }
        let count = model.items.len();
        self.model = Some(model);
        self.scroller.update(cx, |s, cx| {
            s.reset(count, cx);
            s.scroll_to_end(cx);
        });
        cx.notify();
    }

    fn apply_event(&mut self, env: &loft_protocol::EventEnvelope, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        let before = model.items.len();
        if !model.apply_event(env) {
            return;
        }
        let after = model.items.len();
        if let Event::ItemUpserted { item } = &env.event {
            let ix = model.items.iter().position(|i| i.id == item.id);
            let (id, body) = (item.id.clone(), item.body.clone());
            self.sync_markdown(&id, &body, cx);
            self.scroller.update(cx, |s, cx| {
                if after > before {
                    s.append(after - before, cx);
                } else if let Some(ix) = ix {
                    s.remeasure_items(ix..ix + 1, cx);
                }
            });
        }
        cx.notify();
    }

    fn apply_delta(&mut self, delta: &Delta, cx: &mut Context<Self>) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        if !model.apply_delta(delta) {
            return;
        }
        let Some(ix) = model.items.iter().position(|i| i.id == delta.item_id) else {
            return;
        };
        let (id, body) = (model.items[ix].id.clone(), model.items[ix].body.clone());
        // `set_text` appends incrementally when the new text extends the old.
        self.sync_markdown(&id, &body, cx);
        self.scroller.update(cx, |s, cx| {
            s.remeasure_items(ix..ix + 1, cx);
        });
        cx.notify();
    }

    /// Keep a markdown document per assistant message in sync with its text.
    fn sync_markdown(&mut self, id: &ItemId, body: &ItemBody, cx: &mut Context<Self>) {
        let ItemBody::AssistantMessage { text } = body else {
            return;
        };
        match self.markdown.get(id) {
            Some(md) => md.update(cx, |md, cx| md.set_text(text, cx)),
            None => {
                let md = cx.new(|cx| TextViewState::markdown(text, cx).selectable(true));
                self.markdown.insert(id.clone(), md);
            }
        }
    }
}

/// Extension used by views to read the store.
pub fn store_read<'a>(store: &'a Entity<AppStore>, cx: &'a App) -> &'a AppStore {
    store.read(cx)
}
