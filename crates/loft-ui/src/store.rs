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

/// App-wide UI events (e.g. a timeline row asking the panel to open a file).
#[derive(Debug, Clone)]
pub enum StoreEvent {
    OpenFile {
        thread_id: ThreadId,
        rel_path: String,
    },
    OpenAgent {
        thread_id: ThreadId,
        item_id: ItemId,
    },
    /// A browser call arrived from an agent; the panel should show the browser.
    BrowserActivity,
}

impl gpui_kit::EventEmitter<StoreEvent> for AppStore {}

/// Issue a daemon request from any view entity and handle the result there.
pub fn request_in<T: 'static>(
    store: &Entity<AppStore>,
    body: Request,
    cx: &mut Context<T>,
    then: impl FnOnce(&mut T, Result<Response, ErrorBody>, &mut Context<T>) + 'static,
) {
    let fut: std::pin::Pin<Box<dyn Future<Output = Result<Response, ErrorBody>>>> =
        match store.read(cx).client.clone() {
            Some(client) => Box::pin(client.request(body)),
            None => Box::pin(async {
                Err(ErrorBody::new(
                    loft_protocol::ErrorCode::Internal,
                    "not connected to agentloftd",
                ))
            }),
        };
    cx.spawn(async move |this: WeakEntity<T>, cx: &mut AsyncApp| {
        let result = fut.await;
        let _ = this.update(cx, |this, cx| then(this, result, cx));
    })
    .detach();
}

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
    /// Harness for new threads (the last one the user picked).
    pub default_harness: HarnessId,
    /// Messages typed while a turn was running, sent when it ends.
    pub queued: HashMap<ThreadId, std::collections::VecDeque<String>>,
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
            default_harness: HarnessId::claude(),
            queued: HashMap::new(),
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
                &["browser"],
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
                } else if let Some(id) = self.subs.get(&sub_id).cloned()
                    && let Some(entity) = self.threads.get(&id).cloned()
                {
                    entity.update(cx, |t, cx| t.apply_event(&event, cx));
                    self.flush_queue(&id, &entity, cx);
                }
            }
            ClientEvent::Delta { sub_id, delta } => {
                if let Some(id) = self.subs.get(&sub_id)
                    && let Some(entity) = self.threads.get(id)
                {
                    entity.update(cx, |t, cx| t.apply_delta(&delta, cx));
                }
            }
            ClientEvent::TerminalOutput { .. } | ClientEvent::TerminalExited { .. } => {}
            ClientEvent::ClientCall { call_id, .. } => {
                if let Some(client) = &self.client {
                    client.respond_call(
                        call_id,
                        Err(ErrorBody::new(
                            loft_protocol::ErrorCode::Unsupported,
                            "the browser panel is not available in this build",
                        )),
                    );
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

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn open_file(&mut self, thread_id: ThreadId, rel_path: String, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::OpenFile {
            thread_id,
            rel_path,
        });
    }

    pub fn open_agent(&mut self, thread_id: ThreadId, item_id: ItemId, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::OpenAgent { thread_id, item_id });
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
                harness: self.default_harness.clone(),
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

    /// Switch an unstarted thread to another harness (and remember it).
    pub fn set_thread_harness(
        &mut self,
        thread_id: ThreadId,
        harness: HarnessId,
        cx: &mut Context<Self>,
    ) {
        self.default_harness = harness.clone();
        self.request(
            Request::SetThreadHarness { thread_id, harness },
            cx,
            |_, _, _| {},
        );
    }

    /// Queue a message for when the current turn ends.
    pub fn enqueue(&mut self, thread_id: ThreadId, text: String, cx: &mut Context<Self>) {
        self.queued.entry(thread_id).or_default().push_back(text);
        cx.notify();
    }

    pub fn dequeue(&mut self, thread_id: &ThreadId, index: usize, cx: &mut Context<Self>) {
        if let Some(q) = self.queued.get_mut(thread_id) {
            q.remove(index);
        }
        cx.notify();
    }

    /// Send the next queued message once the thread is idle.
    fn flush_queue(&mut self, id: &ThreadId, thread: &Entity<ThreadState>, cx: &mut Context<Self>) {
        let idle = thread.read(cx).model.as_ref().is_some_and(|m| {
            m.running_turn().is_none() && m.thread.status != loft_protocol::ThreadStatus::Running
        });
        if !idle {
            return;
        }
        let Some(text) = self.queued.get_mut(id).and_then(|q| q.pop_front()) else {
            return;
        };
        self.send_message(id.clone(), text, cx);
        cx.notify();
    }

    pub fn send_message(&mut self, thread_id: ThreadId, text: String, cx: &mut Context<Self>) {
        self.request(Request::SendMessage { thread_id, text }, cx, |_, _, _| {});
    }

    pub fn set_permission_mode(
        &mut self,
        thread_id: ThreadId,
        mode: Option<loft_protocol::PermissionMode>,
        cx: &mut Context<Self>,
    ) {
        self.request(
            Request::SetPermissionMode { thread_id, mode },
            cx,
            |_, _, _| {},
        );
    }

    pub fn set_model(
        &mut self,
        thread_id: ThreadId,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.request(Request::SetModel { thread_id, model }, cx, |_, _, _| {});
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

/// A display row in the timeline. Consecutive tool calls fold into one
/// group row; each finished turn gets a footer row.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Item(usize),
    /// A run of top-level tool calls (item indices).
    ToolGroup {
        members: Vec<usize>,
    },
    TurnFooter {
        turn_ix: usize,
    },
}

/// One open thread: its mirrored model plus view-side state (display rows,
/// scroll list, markdown documents, expansion state).
pub struct ThreadState {
    pub id: ThreadId,
    pub model: Option<ThreadModel>,
    pub rows: Vec<Row>,
    pub scroller: Entity<MessageScrollerState>,
    pub markdown: HashMap<ItemId, Entity<TextViewState>>,
    /// Tool groups / reasoning items the user expanded or collapsed,
    /// keyed by their first item id. `true` = expanded.
    pub expanded: HashMap<ItemId, bool>,
}

impl ThreadState {
    fn new(id: ThreadId, cx: &mut Context<Self>) -> Self {
        Self {
            id,
            model: None,
            rows: Vec::new(),
            scroller: cx.new(|cx| MessageScrollerState::new(0, cx)),
            markdown: HashMap::new(),
            expanded: HashMap::new(),
        }
    }

    pub fn toggle(&mut self, key: ItemId, default_open: bool, cx: &mut Context<Self>) {
        let open = self.expanded.get(&key).copied().unwrap_or(default_open);
        self.expanded.insert(key.clone(), !open);
        if let Some(row) = self.row_of_item(&key) {
            self.scroller.update(cx, |s, cx| {
                s.remeasure_items(row..row + 1, cx);
            });
        }
        cx.notify();
    }

    pub fn is_expanded(&self, key: &ItemId, default_open: bool) -> bool {
        self.expanded.get(key).copied().unwrap_or(default_open)
    }

    fn row_of_item(&self, id: &ItemId) -> Option<usize> {
        let model = self.model.as_ref()?;
        let ix = model.items.iter().position(|i| &i.id == id)?;
        self.rows.iter().position(|r| match r {
            Row::Item(i) => *i == ix,
            Row::ToolGroup { members } => members.contains(&ix),
            Row::TurnFooter { .. } => false,
        })
    }

    fn rebuild_rows(&mut self) -> Vec<Row> {
        let Some(model) = self.model.as_ref() else {
            return std::mem::take(&mut self.rows);
        };
        let mut rows = Vec::with_capacity(model.items.len());
        let items = &model.items;
        // Subagent activity lives in its own panel tab, not the main timeline.
        // Claude's internal deferred-tool loading is noise in the timeline.
        let hidden = |i: usize| matches!(&items[i].body, ItemBody::ToolCall { tool, .. } if tool == "ToolSearch");
        let top = |i: usize| items[i].parent_id.is_none() && !hidden(i);
        let is_tool = |i: usize| matches!(items[i].body, ItemBody::ToolCall { .. });
        let mut i = 0;
        while i < items.len() {
            if !top(i) {
                i += 1;
                continue;
            }
            if is_tool(i) {
                // A run of top-level tool calls; nested subagent items in
                // between are skipped over.
                let mut members = Vec::new();
                while i < items.len() && (!top(i) || is_tool(i)) {
                    if top(i) {
                        members.push(i);
                    }
                    i += 1;
                }
                rows.push(Row::ToolGroup { members });
            } else {
                rows.push(Row::Item(i));
                i += 1;
            }
            // Footer after the last item of a finished turn.
            let Some(last) = (0..i).rev().find(|&j| top(j)) else {
                continue;
            };
            let turn_id = &items[last].turn_id;
            let next_turn = items.get(i).map(|n| &n.turn_id);
            if turn_id.is_some()
                && next_turn != Some(turn_id)
                && let Some(turn_ix) = model
                    .turns
                    .iter()
                    .position(|t| Some(&t.id) == turn_id.as_ref() && t.status.is_terminal())
            {
                rows.push(Row::TurnFooter { turn_ix });
            }
        }
        std::mem::replace(&mut self.rows, rows)
    }

    /// Rebuild rows and tell the virtual list what changed.
    fn sync_rows(&mut self, changed_item: Option<&ItemId>, cx: &mut Context<Self>) {
        let old = self.rebuild_rows();
        let new_len = self.rows.len();
        let common = old
            .iter()
            .zip(&self.rows)
            .take_while(|(a, b)| a == b)
            .count();
        let changed_row = changed_item.and_then(|id| self.row_of_item(id));
        self.scroller.update(cx, |s, cx| {
            if common == old.len() {
                // Pure append (the common streaming case).
                if new_len > old.len() {
                    s.append(new_len - old.len(), cx);
                }
            } else {
                // Rows from `common` changed shape (e.g. a tool group grew).
                s.splice(common..old.len(), new_len - common, cx);
            }
            if let Some(row) = changed_row {
                s.remeasure_items(row..row + 1, cx);
            }
        });
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        let Some(model) = ThreadModel::from_snapshot(snapshot) else {
            return;
        };
        self.markdown.clear();
        for item in &model.items {
            self.sync_markdown(&item.id, &item.body, cx);
        }
        self.model = Some(model);
        self.rebuild_rows();
        let count = self.rows.len();
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
        if !model.apply_event(env) {
            return;
        }
        let changed = match &env.event {
            Event::ItemUpserted { item } => {
                let (id, body) = (item.id.clone(), item.body.clone());
                self.sync_markdown(&id, &body, cx);
                Some(id)
            }
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                request.item_id.clone()
            }
            _ => None,
        };
        self.sync_rows(changed.as_ref(), cx);
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
        if let Some(row) = self.row_of_item(&id) {
            self.scroller.update(cx, |s, cx| {
                s.remeasure_items(row..row + 1, cx);
            });
        }
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
