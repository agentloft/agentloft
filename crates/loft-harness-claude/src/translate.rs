//! Pure translation of Claude Code stream-json into canonical harness events.
//!
//! No I/O lives here, so recorded CLI transcripts (`fixtures/`) can be replayed
//! in tests. Unknown message types are ignored (tolerant parsing, PLAN §2.10).

use loft_harness::{HarnessEvent, ToolResult};
use loft_protocol::{
    DecisionKind, DeltaChannel, ItemBody, ItemStatus, NoticeLevel, PermissionMode, Question,
    QuestionOption, RequestKind, RequestReply, ToolKind, TurnStatus, Usage,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// What the translator wants done: emit an event, or write a line to stdin.
#[derive(Debug, Clone)]
pub enum Out {
    Event(HarnessEvent),
    Send(Value),
    /// The answer to a query sent with [`Translator::query`].
    Reply {
        request_id: String,
        result: Result<Value, String>,
    },
}

#[derive(Debug, Clone)]
struct PendingApproval {
    /// The tool asking (`AskUserQuestion` and `ExitPlanMode` answer
    /// differently).
    tool: String,
    input: Value,
    suggestions: Vec<Value>,
    tool_use_id: Option<String>,
}

/// A `show_ui` call whose input is arriving: shown as it is written
/// (ADR-0018), so its item starts before its input is whole.
#[derive(Debug)]
struct ViewCall {
    /// The tool call's id (the item's native id).
    id: String,
    /// The input's JSON as far as it has come.
    json: String,
    /// How much of it the last update covered.
    sent: usize,
}

/// A view being written is passed on each time this much more has come.
const VIEW_STEP: usize = 240;

#[derive(Debug, Default)]
pub struct Translator {
    /// (api message id, block index) → (native item id, channel).
    open_blocks: HashMap<(String, u64), (String, DeltaChannel)>,
    /// Message ids whose text/thinking was streamed via `stream_event`.
    streamed_messages: HashSet<String>,
    /// Tool calls started but without a result yet → their subagent (the
    /// Agent call they run under), if any.
    open_tools: HashMap<String, Option<String>>,
    /// Views being written (`show_ui` calls whose input is still
    /// streaming), by (api message id, block index).
    view_calls: HashMap<(String, u64), ViewCall>,
    /// Tool calls the user denied (their error result becomes `Declined`).
    denied_tools: HashSet<String>,
    pending: HashMap<String, PendingApproval>,
    /// Control requests whose answer someone is waiting for.
    queries: HashSet<String>,
    /// Agent tool call id → the task id Claude runs it as (to stop it).
    tasks: HashMap<String, String>,
    /// Tasks started and not yet finished: task id → what it is.
    open_tasks: std::collections::BTreeMap<String, String>,
    interrupt_requested: bool,
    turn_active: bool,
    next_request: u64,
    /// Messages sent and not yet echoed back (`--replay-user-messages`),
    /// oldest first: `true` for one sent into a running turn.
    unechoed: std::collections::VecDeque<bool>,
    /// Whether Claude echoes our messages (older CLIs may not).
    echoes: bool,
    /// Claude's running cost for the session as the current turn began. It
    /// reports a total, one that a resumed session picks up where it left
    /// off, so a turn's own cost is what the total grew by. `None` while
    /// that isn't known.
    cost_before: Option<f64>,
    /// Our question for that total, asked as the session starts.
    cost_query: Option<String>,
    /// Whether Claude has produced anything in this process: a total that
    /// arrives after that may already count some of the first turn.
    replied: bool,
    /// What the turn used before a message sent mid-turn made it go on.
    /// Claude ends one part and starts the next, with a result for each.
    carried: Option<Usage>,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn turn_active(&self) -> bool {
        self.turn_active
    }

    fn next_request_id(&mut self) -> String {
        self.next_request += 1;
        format!("loft_{}", self.next_request)
    }

    // -- outgoing -----------------------------------------------------------

    pub fn initialize(&mut self) -> Value {
        let id = self.next_request_id();
        json!({"type": "control_request", "request_id": id, "request": {"subtype": "initialize", "hooks": null}})
    }

    /// Ask what the session has cost so far. Sent once, before the first
    /// prompt: Claude answers it before it runs anything.
    pub fn ask_cost_so_far(&mut self) -> Value {
        let id = self.next_request_id();
        self.cost_query = Some(id.clone());
        json!({"type": "control_request", "request_id": id, "request": {"subtype": "get_usage"}})
    }

    pub fn start_turn(&mut self, text: &str) -> Vec<Out> {
        self.start_turn_with(text, &[])
    }

    /// A prompt with images, given as `(mime, base64)`.
    pub fn start_turn_with(&mut self, text: &str, images: &[(String, String)]) -> Vec<Out> {
        self.turn_active = true;
        self.interrupt_requested = false;
        self.unechoed.push_back(false);
        let content = if images.is_empty() {
            json!(text)
        } else {
            let mut blocks: Vec<Value> = images
                .iter()
                .map(|(mime, data)| {
                    json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})
                })
                .collect();
            blocks.push(json!({"type": "text", "text": text}));
            Value::Array(blocks)
        };
        vec![
            Out::Send(json!({
                "type": "user",
                "message": {"role": "user", "content": content},
            })),
            Out::Event(HarnessEvent::TurnStarted),
        ]
    }

    /// A message for the running turn: Claude takes it in at its next step
    /// (the same turn goes on).
    pub fn steer_with(&mut self, text: &str, images: &[(String, String)]) -> Vec<Out> {
        if !self.turn_active {
            return self.start_turn_with(text, images);
        }
        self.unechoed.push_back(true);
        let content = if images.is_empty() {
            json!(text)
        } else {
            let mut blocks: Vec<Value> = images
                .iter()
                .map(|(mime, data)| {
                    json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})
                })
                .collect();
            blocks.push(json!({"type": "text", "text": text}));
            Value::Array(blocks)
        };
        vec![Out::Send(json!({
            "type": "user",
            "message": {"role": "user", "content": content},
        }))]
    }

    pub fn interrupt(&mut self) -> Vec<Out> {
        if !self.turn_active {
            return vec![];
        }
        self.interrupt_requested = true;
        let id = self.next_request_id();
        vec![Out::Send(json!({
            "type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}
        }))]
    }

    /// Ask Claude for a report; the answer comes back as `Out::Reply`
    /// with the returned request id.
    pub fn query(&mut self, kind: loft_protocol::AgentInfoKind) -> (String, Value) {
        let id = self.next_request_id();
        self.queries.insert(id.clone());
        let subtype = match kind {
            loft_protocol::AgentInfoKind::Context => "get_context_usage",
            loft_protocol::AgentInfoKind::Usage | loft_protocol::AgentInfoKind::Status => {
                "get_usage"
            }
        };
        let v = json!({
            "type": "control_request", "request_id": id,
            "request": {"subtype": subtype}
        });
        (id, v)
    }

    /// Stop the subagent started by tool call `tool_id`.
    pub fn stop_subagent(&mut self, tool_id: &str) -> Vec<Out> {
        let Some(task) = self.tasks.get(tool_id).cloned() else {
            return vec![];
        };
        let id = self.next_request_id();
        vec![Out::Send(json!({
            "type": "control_request", "request_id": id,
            "request": {"subtype": "stop_task", "task_id": task}
        }))]
    }

    pub fn set_model(&mut self, model: Option<&str>) -> Vec<Out> {
        let id = self.next_request_id();
        vec![Out::Send(json!({
            "type": "control_request", "request_id": id,
            "request": {"subtype": "set_model", "model": model.unwrap_or("default")}
        }))]
    }

    pub fn set_permission_mode(&mut self, mode: Option<PermissionMode>) -> Vec<Out> {
        let id = self.next_request_id();
        let mode = mode.map(permission_mode_arg).unwrap_or("default");
        vec![Out::Send(json!({
            "type": "control_request", "request_id": id,
            "request": {"subtype": "set_permission_mode", "mode": mode}
        }))]
    }

    pub fn respond(
        &mut self,
        request_id: &str,
        decision: DecisionKind,
        reply: Option<RequestReply>,
    ) -> Vec<Out> {
        let Some(pending) = self.pending.remove(request_id) else {
            return vec![];
        };
        let reply = reply.unwrap_or_default();
        let response = match (pending.tool.as_str(), decision) {
            (_, DecisionKind::DenyAndStop) => {
                self.interrupt_requested = true;
                json!({"behavior": "deny", "message": "The user denied this action and stopped the turn.", "interrupt": true})
            }
            // Answers go back in the tool's input, keyed by question text.
            (QUESTION_TOOL, DecisionKind::AllowOnce | DecisionKind::AllowSession) => {
                let answers: serde_json::Map<String, Value> = reply
                    .answers
                    .iter()
                    .map(|a| (a.id.clone(), json!(a.answers.join(", "))))
                    .collect();
                let mut input = pending.input.clone();
                input["answers"] = Value::Object(answers);
                json!({"behavior": "allow", "updatedInput": input})
            }
            (QUESTION_TOOL, _) => json!({"behavior": "deny", "message": reply
                .message
                .unwrap_or_else(|| "The user chose not to answer. Carry on with your best judgement, or ask in plain text.".into())}),
            // Approving a plan leaves plan mode.
            (PLAN_TOOL, DecisionKind::AllowOnce | DecisionKind::AllowSession) => {
                let mode = if decision == DecisionKind::AllowSession {
                    "acceptEdits"
                } else {
                    "default"
                };
                json!({"behavior": "allow", "updatedInput": pending.input,
                       "updatedPermissions": [{"type": "setMode", "mode": mode, "destination": "session"}]})
            }
            (PLAN_TOOL, _) => {
                let message = match reply.message.as_deref().map(str::trim) {
                    Some(m) if !m.is_empty() => format!(
                        "The user wants changes to the plan before you start. Stay in plan mode, revise the plan, and present it again. Their feedback:\n\n{m}"
                    ),
                    _ => "The user didn't approve the plan. Stay in plan mode and ask what they'd like to change.".to_owned(),
                };
                json!({"behavior": "deny", "message": message})
            }
            (_, DecisionKind::AllowOnce) => {
                json!({"behavior": "allow", "updatedInput": pending.input})
            }
            (_, DecisionKind::AllowSession) => {
                let rules: Vec<Value> = pending
                    .suggestions
                    .iter()
                    .filter(|s| s.get("type").and_then(Value::as_str) == Some("addRules"))
                    .map(|s| {
                        let mut s = s.clone();
                        s["destination"] = json!("session");
                        s
                    })
                    .collect();
                json!({"behavior": "allow", "updatedInput": pending.input, "updatedPermissions": rules})
            }
            (_, DecisionKind::Deny) => {
                json!({"behavior": "deny", "message": "The user denied this action."})
            }
        };
        if matches!(decision, DecisionKind::Deny | DecisionKind::DenyAndStop)
            && let Some(tool) = &pending.tool_use_id
        {
            self.denied_tools.insert(tool.clone());
        }
        vec![
            Out::Send(json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": response}
            })),
            Out::Event(HarnessEvent::RequestResolved {
                native_id: request_id.to_owned(),
            }),
        ]
    }

    // -- incoming -----------------------------------------------------------

    pub fn on_line(&mut self, line: &str) -> Vec<Out> {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.on_value(&v),
            Err(err) => {
                tracing::warn!(%err, line = %truncate(line, 200), "unparseable claude stdout line");
                vec![]
            }
        }
    }

    pub fn on_value(&mut self, v: &Value) -> Vec<Out> {
        match str_at(v, "type") {
            Some("system") => self.on_system(v),
            Some("stream_event") => {
                self.replied = true;
                self.on_stream_event(v)
            }
            Some("assistant") => {
                self.replied = true;
                self.on_assistant(v)
            }
            Some("user") => self.on_user(v),
            Some("control_request") => self.on_control_request(v),
            Some("control_response") => self.on_control_response(v),
            Some("result") => self.on_result(v),
            _ => vec![],
        }
    }

    fn on_system(&mut self, v: &Value) -> Vec<Out> {
        match str_at(v, "subtype") {
            Some("init") => vec![Out::Event(HarnessEvent::SessionReady {
                models: vec![],
                native_session_id: str_at(v, "session_id").unwrap_or_default().to_owned(),
                model: str_at(v, "model").map(str::to_owned),
                slash_commands: v
                    .get("slash_commands")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            })],
            Some("task_started") => {
                if let (Some(task), Some(tool)) = (str_at(v, "task_id"), str_at(v, "tool_use_id")) {
                    self.tasks.insert(tool.to_owned(), task.to_owned());
                }
                match str_at(v, "task_id") {
                    Some(task) => {
                        let what = str_at(v, "description")
                            .or_else(|| str_at(v, "subagent_type"))
                            .unwrap_or("a background task")
                            .to_owned();
                        self.open_tasks.insert(task.to_owned(), what);
                        self.background_tasks()
                    }
                    None => vec![],
                }
            }
            // A task finished (or stopped, or failed).
            Some("task_notification") => match str_at(v, "task_id") {
                Some(task) if self.open_tasks.remove(task).is_some() => self.background_tasks(),
                _ => vec![],
            },
            Some("api_retry") => vec![Out::Event(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: "Claude API error — retrying…".into(),
            })],
            _ => vec![],
        }
    }

    fn background_tasks(&self) -> Vec<Out> {
        vec![Out::Event(HarnessEvent::BackgroundTasks(
            self.open_tasks.values().cloned().collect(),
        ))]
    }

    fn on_stream_event(&mut self, v: &Value) -> Vec<Out> {
        let Some(event) = v.get("event") else {
            return vec![];
        };
        let msg_id = str_at(v, "api_message_id").unwrap_or_default().to_owned();
        let parent = str_at(v, "parent_tool_use_id").map(str::to_owned);
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match str_at(event, "type") {
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                let (body, channel) = match str_at(block, "type") {
                    Some("text") => (
                        ItemBody::AssistantMessage {
                            text: String::new(),
                        },
                        DeltaChannel::Text,
                    ),
                    Some("thinking") => (
                        ItemBody::Reasoning {
                            text: String::new(),
                        },
                        DeltaChannel::Reasoning,
                    ),
                    // A view: its page shows up as the agent writes it.
                    Some("tool_use")
                        if str_at(block, "name").is_some_and(loft_view::is_tool)
                            && str_at(block, "id").is_some() =>
                    {
                        let id = str_at(block, "id").unwrap_or_default().to_owned();
                        let name = str_at(block, "name").unwrap_or_default().to_owned();
                        if self.open_tools.contains_key(&id) {
                            return vec![];
                        }
                        self.open_tools.insert(id.clone(), parent.clone());
                        self.view_calls.insert(
                            (msg_id, index),
                            ViewCall {
                                id: id.clone(),
                                json: String::new(),
                                sent: 0,
                            },
                        );
                        let input = serde_json::json!({ loft_view::PARTIAL: true });
                        return vec![Out::Event(HarnessEvent::ItemStarted {
                            native_id: id,
                            body: ItemBody::ToolCall {
                                title: tool_title(&name, &input),
                                tool_kind: tool_kind(&name),
                                tool: name,
                                input,
                                output: None,
                                is_error: false,
                            },
                            parent,
                        })];
                    }
                    _ => return vec![],
                };
                let native_id = format!("{msg_id}:{index}");
                self.streamed_messages.insert(msg_id.clone());
                self.open_blocks
                    .insert((msg_id, index), (native_id.clone(), channel));
                vec![Out::Event(HarnessEvent::ItemStarted {
                    native_id,
                    body,
                    parent,
                })]
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                if let Some(call) = self.view_calls.get_mut(&(msg_id.clone(), index)) {
                    if str_at(delta, "type") != Some("input_json_delta") {
                        return vec![];
                    }
                    call.json
                        .push_str(str_at(delta, "partial_json").unwrap_or_default());
                    if call.json.len() < call.sent + VIEW_STEP {
                        return vec![];
                    }
                    call.sent = call.json.len();
                    let so_far = loft_view::partial(&call.json);
                    return vec![Out::Event(HarnessEvent::ToolInput {
                        native_id: call.id.clone(),
                        input: serde_json::json!({ "title": so_far.title, "html": so_far.html }),
                        partial: true,
                    })];
                }
                let Some((native_id, channel)) = self.open_blocks.get(&(msg_id, index)) else {
                    return vec![];
                };
                let text = match (str_at(delta, "type"), channel) {
                    (Some("text_delta"), DeltaChannel::Text) => str_at(delta, "text"),
                    (Some("thinking_delta"), DeltaChannel::Reasoning) => str_at(delta, "thinking"),
                    _ => None,
                };
                match text {
                    Some(t) if !t.is_empty() => vec![Out::Event(HarnessEvent::ItemDelta {
                        native_id: native_id.clone(),
                        channel: *channel,
                        text: t.to_owned(),
                    })],
                    _ => vec![],
                }
            }
            Some("content_block_stop")
                if self.view_calls.contains_key(&(msg_id.clone(), index)) =>
            {
                let Some(call) = self.view_calls.remove(&(msg_id, index)) else {
                    return vec![];
                };
                // Whole now: as the agent wrote it, or (should the JSON not
                // read) as much of it as did.
                let input = serde_json::from_str::<Value>(&call.json).unwrap_or_else(|_| {
                    let view = loft_view::partial(&call.json);
                    serde_json::json!({ "title": view.title, "html": view.html })
                });
                vec![Out::Event(HarnessEvent::ToolInput {
                    native_id: call.id,
                    input,
                    partial: false,
                })]
            }
            Some("content_block_stop") => match self.open_blocks.remove(&(msg_id, index)) {
                Some((native_id, _)) => vec![Out::Event(HarnessEvent::ItemCompleted {
                    native_id,
                    body: None,
                    tool_result: None,
                    status: ItemStatus::Completed,
                })],
                None => vec![],
            },
            _ => vec![],
        }
    }

    fn on_assistant(&mut self, v: &Value) -> Vec<Out> {
        let message = v.get("message").unwrap_or(&Value::Null);
        let msg_id = str_at(message, "id").unwrap_or_default().to_owned();
        let parent = str_at(v, "parent_tool_use_id").map(str::to_owned);
        let streamed = self.streamed_messages.contains(&msg_id);
        let mut out = Vec::new();
        // The latest main-thread message marks where a fork can resume.
        if parent.is_none()
            && let Some(uuid) = str_at(v, "uuid")
        {
            out.push(Out::Event(HarnessEvent::TurnMarker {
                native_turn: uuid.to_owned(),
            }));
        }
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return out;
        };
        for (i, block) in content.iter().enumerate() {
            match str_at(block, "type") {
                Some("text") if !streamed => {
                    let native_id = format!("{msg_id}:{i}");
                    let text = str_at(block, "text").unwrap_or_default().to_owned();
                    out.push(Out::Event(HarnessEvent::ItemStarted {
                        parent: parent.clone(),
                        native_id: native_id.clone(),
                        body: ItemBody::AssistantMessage {
                            text: String::new(),
                        },
                    }));
                    out.push(Out::Event(HarnessEvent::ItemCompleted {
                        native_id,
                        body: Some(ItemBody::AssistantMessage { text }),
                        tool_result: None,
                        status: ItemStatus::Completed,
                    }));
                }
                Some("thinking") if !streamed => {
                    let native_id = format!("{msg_id}:{i}");
                    let text = str_at(block, "thinking").unwrap_or_default().to_owned();
                    out.push(Out::Event(HarnessEvent::ItemStarted {
                        parent: parent.clone(),
                        native_id: native_id.clone(),
                        body: ItemBody::Reasoning {
                            text: String::new(),
                        },
                    }));
                    out.push(Out::Event(HarnessEvent::ItemCompleted {
                        native_id,
                        body: Some(ItemBody::Reasoning { text }),
                        tool_result: None,
                        status: ItemStatus::Completed,
                    }));
                }
                Some("tool_use") => {
                    let Some(id) = str_at(block, "id") else {
                        continue;
                    };
                    if self.open_tools.contains_key(id) {
                        continue;
                    }
                    self.open_tools.insert(id.to_owned(), parent.clone());
                    let name = str_at(block, "name").unwrap_or("tool").to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    out.push(Out::Event(HarnessEvent::ItemStarted {
                        parent: parent.clone(),
                        native_id: id.to_owned(),
                        body: ItemBody::ToolCall {
                            title: tool_title(&name, &input),
                            tool_kind: tool_kind(&name),
                            tool: name,
                            input,
                            output: None,
                            is_error: false,
                        },
                    }));
                }
                _ => {}
            }
        }
        out
    }

    fn on_user(&mut self, v: &Value) -> Vec<Out> {
        // Our own message echoed back: Claude has taken it in.
        if v.get("isReplay").and_then(Value::as_bool) == Some(true) {
            // Claude's own note about a command it ran for us ("Set model
            // to …" after `set_model`) comes back the same way. It is not
            // a message of ours and nothing answers it.
            if v.pointer("/message/content")
                .and_then(Value::as_str)
                .is_some_and(|text| {
                    let text = text.trim_start();
                    text.starts_with("<local-command-") || text.starts_with("<command-")
                })
            {
                return vec![];
            }
            self.echoes = true;
            let ours = self.unechoed.pop_front().is_some();
            if ours && !self.turn_active {
                // A message sent mid-turn that Claude only got to after the
                // turn ended: it runs as a turn of its own.
                self.turn_active = true;
                self.interrupt_requested = false;
                return vec![Out::Event(HarnessEvent::TurnStarted)];
            }
            return vec![];
        }
        let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
            // A plain-string user message is our own prompt echoed back.
            return vec![];
        };
        let mut out = Vec::new();
        for block in content {
            if str_at(block, "type") != Some("tool_result") {
                continue;
            }
            let Some(tool_id) = str_at(block, "tool_use_id") else {
                continue;
            };
            if self.open_tools.remove(tool_id).is_none() {
                continue;
            }
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let status = if self.denied_tools.remove(tool_id) {
                ItemStatus::Declined
            } else if is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id: tool_id.to_owned(),
                body: None,
                tool_result: Some(ToolResult {
                    output: tool_result_text(block.get("content")),
                    is_error,
                }),
                status,
            }));
        }
        out
    }

    fn on_control_request(&mut self, v: &Value) -> Vec<Out> {
        let request_id = str_at(v, "request_id").unwrap_or_default().to_owned();
        let request = v.get("request").unwrap_or(&Value::Null);
        match str_at(request, "subtype") {
            Some("can_use_tool") => {
                let tool = str_at(request, "tool_name").unwrap_or("tool").to_owned();
                let input = request.get("input").cloned().unwrap_or(Value::Null);
                let display = str_at(request, "display_name").unwrap_or(&tool).to_owned();
                let summary = str_at(request, "description")
                    .map(str::to_owned)
                    .unwrap_or_else(|| tool_title(&tool, &input));
                let tool_use_id = str_at(request, "tool_use_id").map(str::to_owned);
                self.pending.insert(
                    request_id.clone(),
                    PendingApproval {
                        tool: tool.clone(),
                        input: input.clone(),
                        suggestions: request
                            .get("permission_suggestions")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                        tool_use_id: tool_use_id.clone(),
                    },
                );
                if tool == QUESTION_TOOL {
                    let questions = questions(&input);
                    let first = questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default();
                    return vec![Out::Event(HarnessEvent::RequestOpened {
                        native_id: request_id,
                        kind: RequestKind::Question,
                        title: format!("{display}: {first}"),
                        detail: json!({ "questions": questions }),
                        options: vec![
                            DecisionKind::AllowOnce,
                            DecisionKind::Deny,
                            DecisionKind::DenyAndStop,
                        ],
                        related_item: tool_use_id,
                    })];
                }
                if tool == PLAN_TOOL {
                    let plan = str_at(&input, "plan").unwrap_or_default();
                    return vec![Out::Event(HarnessEvent::RequestOpened {
                        native_id: request_id,
                        kind: RequestKind::Plan,
                        title: format!("{display}: Plan"),
                        detail: json!({ "plan": plan }),
                        options: vec![
                            DecisionKind::AllowSession,
                            DecisionKind::AllowOnce,
                            DecisionKind::Deny,
                            DecisionKind::DenyAndStop,
                        ],
                        related_item: tool_use_id,
                    })];
                }
                let kind = match tool_kind(&tool) {
                    ToolKind::FileChange => RequestKind::FileChangeApproval,
                    _ => RequestKind::ToolApproval,
                };
                vec![Out::Event(HarnessEvent::RequestOpened {
                    native_id: request_id,
                    kind,
                    title: format!("{display}: {summary}"),
                    detail: json!({
                        "tool": tool,
                        "input": input,
                        "blocked_path": request.get("blocked_path"),
                        "decision_reason": request.get("decision_reason"),
                    }),
                    options: vec![
                        DecisionKind::AllowOnce,
                        DecisionKind::AllowSession,
                        DecisionKind::Deny,
                        DecisionKind::DenyAndStop,
                    ],
                    related_item: tool_use_id,
                })]
            }
            other => {
                // Hooks / SDK MCP aren't registered by us; answer so the CLI
                // never blocks waiting.
                tracing::debug!(subtype = ?other, "unsupported claude control_request");
                vec![Out::Send(json!({
                    "type": "control_response",
                    "response": {"subtype": "error", "request_id": request_id,
                                 "error": format!("unsupported control request: {}", other.unwrap_or("?"))}
                }))]
            }
        }
    }

    fn on_control_response(&mut self, v: &Value) -> Vec<Out> {
        let id = v.pointer("/response/request_id").and_then(Value::as_str);
        if id.is_some() && id == self.cost_query.as_deref() {
            // Ours, and nobody is waiting for it: a CLI too old to answer
            // just leaves the first turn without a cost.
            self.cost_query = None;
            if self.cost_before.is_none() && !self.replied {
                self.cost_before = v
                    .pointer("/response/response/session/total_cost_usd")
                    .and_then(Value::as_f64);
            }
            return vec![];
        }
        if let Some(id) = id
            && self.queries.remove(id)
        {
            let result = if v.pointer("/response/subtype").and_then(Value::as_str) == Some("error")
            {
                Err(v
                    .pointer("/response/error")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude couldn't answer")
                    .to_owned())
            } else {
                Ok(v.pointer("/response/response")
                    .cloned()
                    .unwrap_or(Value::Null))
            };
            return vec![Out::Reply {
                request_id: id.to_owned(),
                result,
            }];
        }
        if v.pointer("/response/response/commands").is_some() {
            let commands = commands(v);
            if !commands.is_empty() {
                return vec![Out::Event(HarnessEvent::CommandsAvailable(commands))];
            }
        }
        if v.pointer("/response/subtype").and_then(Value::as_str) == Some("error") {
            let msg = v
                .pointer("/response/error")
                .and_then(Value::as_str)
                .unwrap_or("control request failed");
            return vec![Out::Event(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: format!("Claude: {msg}"),
            })];
        }
        vec![]
    }

    fn on_result(&mut self, v: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        // A message sent mid-turn that Claude hasn't taken in yet: it goes
        // on with it straight away, so this turn goes on too.
        let goes_on =
            self.echoes && !self.interrupt_requested && self.unechoed.iter().any(|steer| *steer);
        if !self.echoes {
            self.unechoed.clear();
        }
        // Anything still open is finished now.
        for ((_, _), (native_id, _)) in self.open_blocks.drain() {
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id,
                body: None,
                tool_result: None,
                status: ItemStatus::Completed,
            }));
        }
        // A subagent's steps outlive the turn when it runs in the background;
        // they stay open (and get their results later) unless the user
        // stopped the turn.
        let interrupted = self.interrupt_requested;
        let ended: Vec<String> = self
            .open_tools
            .iter()
            .filter(|(_, parent)| parent.is_none() || interrupted)
            .map(|(id, _)| id.clone())
            .collect();
        for tool in ended {
            self.open_tools.remove(&tool);
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id: tool,
                body: None,
                tool_result: None,
                status: ItemStatus::Failed,
            }));
        }
        for (request_id, _) in self.pending.drain() {
            out.push(Out::Event(HarnessEvent::RequestResolved {
                native_id: request_id,
            }));
        }
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let subtype = str_at(v, "subtype").unwrap_or("success");
        let status = if self.interrupt_requested {
            TurnStatus::Interrupted
        } else if subtype == "success" && !is_error {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        };
        let error = (status == TurnStatus::Failed).then(|| {
            str_at(v, "result")
                .filter(|s| !s.is_empty())
                .unwrap_or(subtype)
                .to_owned()
        });
        // Tokens and time are this result's own; the cost is the session's.
        let usage = v.get("usage").map(|u| Usage {
            input_tokens: u64_at(u, "input_tokens"),
            output_tokens: u64_at(u, "output_tokens"),
            cache_read_tokens: u64_at(u, "cache_read_input_tokens"),
            cache_write_tokens: u64_at(u, "cache_creation_input_tokens"),
            cost_usd: None,
            duration_ms: v.get("duration_ms").and_then(Value::as_u64),
        });
        self.streamed_messages.clear();
        self.denied_tools.clear();
        let mut usage = add_usage(self.carried.take(), usage);
        if goes_on {
            self.carried = usage;
            return out;
        }
        if let Some(total) = v.get("total_cost_usd").and_then(Value::as_f64) {
            let cost = self.cost_before.map(|before| {
                // A total below the last one is a count that started over.
                if total >= before {
                    total - before
                } else {
                    total
                }
            });
            self.cost_before = Some(total);
            if let Some(usage) = usage.as_mut() {
                usage.cost_usd = cost;
            }
        }
        self.turn_active = false;
        self.interrupt_requested = false;
        out.push(Out::Event(HarnessEvent::TurnCompleted {
            status,
            usage,
            error,
        }));
        out
    }
}

/// Claude's tool for asking the user multiple-choice questions.
pub const QUESTION_TOOL: &str = "AskUserQuestion";
/// Claude's tool for presenting a plan and leaving plan mode.
pub const PLAN_TOOL: &str = "ExitPlanMode";

/// `AskUserQuestion`'s questions in canonical form: keyed by their text
/// (Claude's answers are), with a typed answer always allowed.
pub fn questions(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let question = str_at(q, "question")?.to_owned();
            Some(Question {
                id: question.clone(),
                header: str_at(q, "header").unwrap_or_default().to_owned(),
                question,
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        Some(QuestionOption {
                            label: str_at(o, "label")?.to_owned(),
                            description: str_at(o, "description").unwrap_or_default().to_owned(),
                        })
                    })
                    .collect(),
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                allow_other: true,
                secret: false,
            })
        })
        .collect()
}

/// Two parts of one turn, added up.
fn add_usage(a: Option<Usage>, b: Option<Usage>) -> Option<Usage> {
    let (Some(a), Some(b)) = (&a, &b) else {
        return a.or(b);
    };
    Some(Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_read_tokens: a.cache_read_tokens + b.cache_read_tokens,
        cache_write_tokens: a.cache_write_tokens + b.cache_write_tokens,
        cost_usd: None,
        duration_ms: match (a.duration_ms, b.duration_ms) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
    })
}

pub fn permission_mode_arg(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "plan",
        PermissionMode::AskAll => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "bypassPermissions",
    }
}

pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" | "BashOutput" | "KillShell" | "KillBash" => ToolKind::Command,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::FileChange,
        "Read" | "NotebookRead" | "LS" => ToolKind::FileRead,
        "Grep" | "Glob" | "WebSearch" | "ToolSearch" => ToolKind::Search,
        "WebFetch" => ToolKind::WebFetch,
        "Task" | "Agent" => ToolKind::Subagent,
        "TodoWrite" => ToolKind::Todo,
        n if n.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

/// One-line human detail for a tool call (the command, path, pattern…).
/// The tool name itself is shown separately by the UI.
pub fn tool_title(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| str_at(input, k).map(str::to_owned))
    };
    let detail = match tool_kind(name) {
        ToolKind::Command => pick(&["command"]),
        ToolKind::FileChange | ToolKind::FileRead => pick(&["file_path", "notebook_path", "path"]),
        ToolKind::Search => pick(&["pattern", "query"]),
        ToolKind::WebFetch => pick(&["url"]),
        ToolKind::Subagent => pick(&["description", "prompt"]),
        _ => None,
    };
    match detail {
        Some(d) => truncate(&d.replace('\n', " "), 200),
        None => name.to_owned(),
    }
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| str_at(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The slash commands in an `initialize` response.
pub fn commands(v: &Value) -> Vec<loft_protocol::SlashCommandInfo> {
    v.pointer("/response/response/commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = str_at(c, "name")?.to_owned();
            Some(loft_protocol::SlashCommandInfo {
                name,
                description: str_at(c, "description").unwrap_or_default().to_owned(),
                argument_hint: str_at(c, "argumentHint").unwrap_or_default().to_owned(),
                builtin: c.get("builtin").and_then(Value::as_bool).unwrap_or(false),
                insert: String::new(),
            })
        })
        .collect()
}

/// The model catalog in an `initialize` control response. Claude's
/// `default` row only names the recommended model; it marks that model as
/// the default instead of being listed itself.
pub fn models(response: &Value) -> Vec<loft_protocol::ModelInfo> {
    let rows: Vec<&Value> = response
        .pointer("/response/response/models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| !m["disabled"].as_bool().unwrap_or(false))
        .collect();
    let default_model = rows
        .iter()
        .find(|m| m["value"] == "default")
        .and_then(|m| m["resolvedModel"].as_str());
    let mut default_marked = false;
    rows.iter()
        .filter(|m| m["value"] != "default")
        .filter_map(|m| {
            let id = m["value"].as_str()?;
            let resolved = m["resolvedModel"].as_str();
            let is_default =
                !default_marked && default_model.is_some() && resolved == default_model;
            default_marked |= is_default;
            Some(loft_protocol::ModelInfo {
                id: id.to_owned(),
                display_name: m["displayName"].as_str().unwrap_or(id).to_owned(),
                description: m["description"].as_str().map(str::to_owned),
                is_default,
                resolved_id: resolved.filter(|r| *r != id).map(str::to_owned),
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect(),
                default_effort: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_catalog_from_initialize() {
        let v = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "loft_1", "response": {"models": [
            {"value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)"},
            {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus 5.5", "description": "Complex work",
             "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]},
            {"value": "claude-haiku-4-5", "resolvedModel": "claude-haiku-4-5", "displayName": "Haiku 4.5"},
            {"value": "old", "displayName": "Old", "disabled": true}
        ]}}});
        let models = models(&v);
        assert_eq!(models.len(), 2, "default row folded in, disabled dropped");
        assert_eq!(models[0].id, "opus");
        assert!(models[0].is_default);
        assert_eq!(models[0].resolved_id.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(models[0].efforts.len(), 5);
        assert!(!models[1].is_default);
        assert_eq!(models[1].resolved_id, None, "same as the id");
        assert!(models[1].efforts.is_empty());
    }

    fn replay(fixture: &str) -> (Translator, Vec<Out>) {
        let mut t = Translator::new();
        let mut out = t.start_turn("prompt");
        for line in fixture.lines().filter(|l| !l.trim().is_empty()) {
            out.extend(t.on_line(line));
        }
        (t, out)
    }

    fn events(out: &[Out]) -> Vec<&HarnessEvent> {
        out.iter()
            .filter_map(|o| match o {
                Out::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_view_is_passed_on_while_it_is_written() {
        let mut t = Translator::new();
        let mut out = t.start_turn("prompt");
        let event = |event: Value| {
            serde_json::json!({ "type": "stream_event", "api_message_id": "msg_1", "event": event })
                .to_string()
        };
        let html = format!("<h1>Sizes</h1>{}", "<p>row</p>".repeat(80));
        let whole = serde_json::json!({ "title": "Sizes", "html": html });
        let json = whole.to_string();
        out.extend(t.on_line(&event(serde_json::json!({
            "type": "content_block_start", "index": 1,
            "content_block": { "type": "tool_use", "id": "toolu_1",
                               "name": "mcp__agentloft__show_ui", "input": {} }
        }))));
        // The input arrives a few characters at a time.
        for piece in json.as_bytes().chunks(37) {
            out.extend(t.on_line(&event(serde_json::json!({
                "type": "content_block_delta", "index": 1,
                "delta": { "type": "input_json_delta",
                           "partial_json": std::str::from_utf8(piece).unwrap() }
            }))));
        }
        out.extend(t.on_line(&event(
            serde_json::json!({ "type": "content_block_stop", "index": 1 }),
        )));
        // The message itself, whole: its tool call is the one already started.
        out.extend(
            t.on_line(
                &serde_json::json!({
                    "type": "assistant",
                    "message": { "id": "msg_1", "content": [
                        { "type": "tool_use", "id": "toolu_1",
                          "name": "mcp__agentloft__show_ui", "input": whole }
                    ]}
                })
                .to_string(),
            ),
        );
        let evs = events(&out);
        let started: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemStarted {
                    native_id, body, ..
                } => Some((native_id, body)),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1, "one item, started once");
        assert_eq!(started[0].0, "toolu_1");
        assert!(matches!(started[0].1, ItemBody::ToolCall { input, .. }
            if input[loft_view::PARTIAL] == true));
        let inputs: Vec<(&Value, bool)> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ToolInput { input, partial, .. } => Some((input, *partial)),
                _ => None,
            })
            .collect();
        let (last, earlier) = inputs.split_last().unwrap();
        assert!(earlier.len() >= 2, "it was passed on as it grew");
        let mut seen = 0;
        for (input, partial) in earlier {
            assert!(partial);
            let so_far = input["html"].as_str().unwrap();
            assert!(html.starts_with(so_far) && so_far.len() >= seen);
            seen = so_far.len();
        }
        assert_eq!(*last, (&whole, false), "and once more, whole");
    }

    #[test]
    fn text_turn_streams_and_completes() {
        let (_, out) = replay(include_str!("../fixtures/2.1.319-text-turn.jsonl"));
        let evs = events(&out);
        assert!(matches!(evs[0], HarnessEvent::TurnStarted));
        assert!(evs.iter().any(|e| matches!(e, HarnessEvent::SessionReady { native_session_id, .. } if native_session_id == "11111111-2222-4333-8444-555555555556")));
        let streamed: String = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed.trim(), "pong");
        // The full assistant message must not duplicate the streamed item.
        let started = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ItemStarted { .. }))
            .count();
        assert_eq!(started, 1);
        match evs.last().unwrap() {
            HarnessEvent::TurnCompleted { status, usage, .. } => {
                assert_eq!(*status, TurnStatus::Completed);
                let usage = usage.as_ref().unwrap();
                assert_eq!((usage.output_tokens, usage.duration_ms), (4, Some(1219)));
                // Nothing said what the session had cost before this turn.
                assert_eq!(usage.cost_usd, None);
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn tool_approval_round_trip() {
        let fixture = include_str!("../fixtures/2.1.319-tool-approval.jsonl");
        let mut t = Translator::new();
        let mut out = t.start_turn("prompt");
        let mut request_id = None;
        for line in fixture.lines().filter(|l| !l.trim().is_empty()) {
            let produced = t.on_line(line);
            for o in &produced {
                if let Out::Event(HarnessEvent::RequestOpened {
                    native_id,
                    title,
                    options,
                    related_item,
                    ..
                }) = o
                {
                    assert!(title.starts_with("Bash:"), "{title}");
                    assert_eq!(options.len(), 4);
                    assert!(related_item.is_some());
                    request_id = Some(native_id.clone());
                }
            }
            out.extend(produced);
            // Answer the approval right where the CLI asked.
            if let Some(id) = request_id.take() {
                let reply = t.respond(&id, DecisionKind::AllowSession, None);
                match &reply[0] {
                    Out::Send(v) => {
                        assert_eq!(v["response"]["response"]["behavior"], "allow");
                        let perms = v["response"]["response"]["updatedPermissions"]
                            .as_array()
                            .unwrap();
                        assert_eq!(perms.len(), 1);
                        assert_eq!(perms[0]["destination"], "session");
                    }
                    other => panic!("expected Send, got {other:?}"),
                }
                out.extend(reply);
            }
        }
        let evs = events(&out);
        let tool_done = evs.iter().find_map(|e| match e {
            HarnessEvent::ItemCompleted {
                tool_result: Some(r),
                status,
                ..
            } => Some((r, *status)),
            _ => None,
        });
        let (_, status) = tool_done.expect("tool call completed");
        assert_eq!(status, ItemStatus::Completed);
        assert!(
            evs.iter()
                .any(|e| matches!(e, HarnessEvent::RequestResolved { .. }))
        );
        assert!(matches!(
            evs.last().unwrap(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
    }

    fn completed(out: &[Out]) -> bool {
        events(out)
            .iter()
            .any(|e| matches!(e, HarnessEvent::TurnCompleted { .. }))
    }

    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;

    #[test]
    fn a_message_sent_mid_turn_keeps_the_turn_going() {
        let mut t = Translator::new();
        t.start_turn("write a story");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"write a story"}}"#);
        // Into the running turn: no new turn starts.
        let out = t.steer_with("now say BANANA", &[]);
        assert!(matches!(&out[..], [Out::Send(v)] if v["message"]["content"] == "now say BANANA"));
        // Claude finished before taking it in: it goes on with it, so the
        // turn isn't over yet.
        assert!(!completed(&t.on_line(RESULT)));
        assert!(t.turn_active());
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"now say BANANA"}}"#);
        assert!(completed(&t.on_line(RESULT)));
        assert!(!t.turn_active());
    }

    #[test]
    fn a_message_taken_in_mid_turn_ends_with_the_turn() {
        let mut t = Translator::new();
        t.start_turn("run a command");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"run a command"}}"#);
        t.steer_with("also say BANANA", &[]);
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"also say BANANA"}}"#);
        assert!(completed(&t.on_line(RESULT)));
    }

    #[test]
    fn without_echoes_turns_end_as_before() {
        let mut t = Translator::new();
        t.start_turn("x");
        t.steer_with("y", &[]);
        assert!(completed(&t.on_line(RESULT)));
    }

    /// A result: what it used itself, and the session's total so far.
    fn result(tokens: u64, ms: u64, total: f64) -> String {
        json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "ok",
            "duration_ms": ms, "total_cost_usd": total,
            "usage": {"input_tokens": tokens, "output_tokens": 1},
        })
        .to_string()
    }

    fn used(out: &[Out]) -> Usage {
        events(out)
            .iter()
            .find_map(|e| match e {
                HarnessEvent::TurnCompleted { usage, .. } => usage.clone(),
                _ => None,
            })
            .expect("a finished turn with its usage")
    }

    /// The session's total, as Claude answers our question at the start.
    fn cost_so_far(t: &mut Translator, total: f64) -> Vec<Out> {
        let ask = t.ask_cost_so_far();
        t.on_value(&json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": ask["request_id"],
            "response": {"session": {"total_cost_usd": total}},
        }}))
    }

    fn cents(usage: &Usage) -> Option<i64> {
        usage.cost_usd.map(|cost| (cost * 100.0).round() as i64)
    }

    #[test]
    fn a_turn_costs_what_the_session_total_grew_by() {
        let mut t = Translator::new();
        // A resumed session: it had already cost half a dollar.
        assert!(cost_so_far(&mut t, 0.50).is_empty());
        t.start_turn("x");
        assert_eq!(cents(&used(&t.on_line(&result(10, 900, 0.75)))), Some(25));
        t.start_turn("y");
        assert_eq!(cents(&used(&t.on_line(&result(10, 900, 0.85)))), Some(10));
    }

    #[test]
    fn a_turn_has_no_cost_while_the_total_before_it_is_unknown() {
        let mut t = Translator::new();
        let ask = t.ask_cost_so_far();
        t.start_turn("x");
        t.on_line(r#"{"type":"assistant","message":{"id":"m1","content":[]}}"#);
        // Too late: this total may count some of the turn under way.
        let late = t.on_value(&json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": ask["request_id"],
            "response": {"session": {"total_cost_usd": 2.0}},
        }}));
        assert!(late.is_empty());
        assert_eq!(used(&t.on_line(&result(10, 900, 2.25))).cost_usd, None);
        // From here on the total is known.
        t.start_turn("y");
        assert_eq!(cents(&used(&t.on_line(&result(10, 900, 2.50)))), Some(25));
    }

    #[test]
    fn a_claude_that_cannot_say_its_cost_is_not_a_warning() {
        let mut t = Translator::new();
        let ask = t.ask_cost_so_far();
        let out = t.on_value(&json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": ask["request_id"], "error": "unknown request",
        }}));
        assert!(out.is_empty());
    }

    #[test]
    fn a_turn_that_went_on_counts_all_of_its_parts() {
        let mut t = Translator::new();
        cost_so_far(&mut t, 1.0);
        t.start_turn("work for a while");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"work for a while"}}"#);
        t.steer_with("stop now", &[]);
        // Claude ends its part, then takes the message in and answers it.
        assert!(!completed(&t.on_line(&result(100, 12_900, 1.07))));
        t.on_line(
            r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"stop now"}}"#,
        );
        let usage = used(&t.on_line(&result(20, 1_300, 1.09)));
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.duration_ms, Some(14_200));
        assert_eq!(cents(&usage), Some(9));
        // The next turn starts from nothing carried over.
        t.start_turn("again");
        let usage = used(&t.on_line(&result(5, 400, 1.10)));
        assert_eq!((usage.input_tokens, usage.duration_ms), (5, Some(400)));
        assert_eq!(cents(&usage), Some(1));
    }

    #[test]
    fn a_message_left_over_after_a_stop_runs_as_its_own_turn() {
        let mut t = Translator::new();
        t.start_turn("x");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"x"}}"#);
        t.steer_with("y", &[]);
        t.interrupt();
        assert!(completed(&t.on_line(RESULT)));
        let out =
            t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"y"}}"#);
        assert!(matches!(events(&out)[..], [HarnessEvent::TurnStarted]));
        assert!(t.turn_active());
    }

    #[test]
    fn a_model_change_after_a_turn_starts_no_turn() {
        let mut t = Translator::new();
        t.start_turn("x");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"x"}}"#);
        assert!(completed(&t.on_line(RESULT)));
        t.set_model(Some("sonnet"));
        let out = t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"<local-command-stdout>Set model to `sonnet (claude-sonnet-5-5)`</local-command-stdout>"}}"#);
        assert!(events(&out).is_empty());
        assert!(!t.turn_active());
        // Mid-turn, the note doesn't stand in for a message still on its way.
        t.start_turn("x");
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"x"}}"#);
        t.steer_with("y", &[]);
        t.on_line(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"<local-command-stdout>Set model to `opus`</local-command-stdout>"}}"#);
        assert!(!completed(&t.on_line(RESULT)), "the steer is still to come");
    }

    #[test]
    fn subagent_items_carry_their_parent() {
        let mut t = Translator::new();
        t.start_turn("x");
        let mut agent_id = None;
        let mut child_parent = None;
        for line in include_str!("../fixtures/2.1.319-subagent.jsonl").lines() {
            for out in t.on_line(line) {
                let Out::Event(HarnessEvent::ItemStarted {
                    native_id,
                    body,
                    parent,
                }) = out
                else {
                    // Auto-allow any approval the subagent asks for.
                    continue;
                };
                if let ItemBody::ToolCall {
                    tool_kind, tool, ..
                } = &body
                {
                    if *tool_kind == ToolKind::Subagent {
                        agent_id = Some(native_id.clone());
                    } else if tool == "Bash" {
                        child_parent = parent.clone();
                    }
                }
            }
        }
        let agent_id = agent_id.expect("an Agent tool call");
        assert_eq!(child_parent.as_deref(), Some(agent_id.as_str()));
    }

    #[test]
    fn initialize_response_lists_commands() {
        let mut t = Translator::new();
        let out = t.on_line(r#"{"type":"control_response","response":{"subtype":"success","request_id":"loft_1","response":{"commands":[{"name":"compact","description":"Free up context","argumentHint":"<instructions>","builtin":true},{"name":"pdf","description":"Work with PDFs"}]}}}"#);
        let [Out::Event(HarnessEvent::CommandsAvailable(cmds))] = out.as_slice() else {
            panic!("expected the commands");
        };
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "compact");
        assert_eq!(cmds[0].argument_hint, "<instructions>");
        assert!(cmds[0].builtin && !cmds[1].builtin);
    }

    #[test]
    fn stopping_a_subagent_sends_stop_task() {
        let mut t = Translator::new();
        assert!(t.stop_subagent("toolu_1").is_empty());
        t.on_line(
            r#"{"type":"system","subtype":"task_started","task_id":"t9","tool_use_id":"toolu_1"}"#,
        );
        let out = t.stop_subagent("toolu_1");
        let [Out::Send(v)] = out.as_slice() else {
            panic!("expected one control request");
        };
        assert_eq!(v["request"]["subtype"], "stop_task");
        assert_eq!(v["request"]["task_id"], "t9");
    }

    #[test]
    fn open_tasks_are_reported_until_they_finish() {
        let mut t = Translator::new();
        let started = |id: &str, what: &str| {
            format!(
                r#"{{"type":"system","subtype":"task_started","task_id":"{id}","tool_use_id":"tu_{id}","description":"{what}","is_backgrounded":true}}"#
            )
        };
        let out = t.on_line(&started("a", "sleep 30"));
        assert!(matches!(events(&out).as_slice(),
            [HarnessEvent::BackgroundTasks(v)] if v == &vec!["sleep 30".to_owned()]));
        t.on_line(&started("b", "Review the parser"));
        let out = t.on_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"a","status":"completed"}"#,
        );
        assert!(matches!(events(&out).as_slice(),
            [HarnessEvent::BackgroundTasks(v)] if v == &vec!["Review the parser".to_owned()]));
        // A repeat notice changes nothing.
        let out = t.on_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"a","status":"completed"}"#,
        );
        assert!(events(&out).is_empty());
        let out = t.on_line(
            r#"{"type":"system","subtype":"task_notification","task_id":"b","status":"stopped"}"#,
        );
        assert!(matches!(events(&out).as_slice(),
            [HarnessEvent::BackgroundTasks(v)] if v.is_empty()));
    }

    fn sent(out: &[Out]) -> Value {
        out.iter()
            .find_map(|o| match o {
                Out::Send(v) => Some(v.clone()),
                _ => None,
            })
            .expect("a line to Claude")
    }

    #[test]
    fn ask_user_question_opens_a_question_and_sends_answers_back() {
        let mut t = Translator::new();
        let out = t.on_line(
            r#"{"type":"control_request","request_id":"q1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"tu1","input":{"questions":[{"question":"Which DB?","header":"DB","multiSelect":false,"options":[{"label":"SQLite","description":"a file"},{"label":"Postgres","description":"a server"}]},{"question":"Extras?","header":"Extras","multiSelect":true,"options":[{"label":"Tests","description":""},{"label":"Docs","description":""}]}]}}}"#,
        );
        let [HarnessEvent::RequestOpened { kind, detail, .. }] = events(&out).as_slice() else {
            panic!("expected a request: {out:?}");
        };
        assert_eq!(*kind, RequestKind::Question);
        let qs: Vec<Question> = serde_json::from_value(detail["questions"].clone()).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].id, "Which DB?");
        assert_eq!(qs[0].options[1].label, "Postgres");
        assert!(qs[1].multi_select && qs[1].allow_other);

        let reply = RequestReply {
            answers: vec![
                loft_protocol::QuestionAnswer {
                    id: "Which DB?".into(),
                    answers: vec!["SQLite".into()],
                },
                loft_protocol::QuestionAnswer {
                    id: "Extras?".into(),
                    answers: vec!["Tests".into(), "lint".into()],
                },
            ],
            message: None,
        };
        let v = sent(&t.respond("q1", DecisionKind::AllowOnce, Some(reply)));
        let r = &v["response"]["response"];
        assert_eq!(r["behavior"], "allow");
        assert_eq!(r["updatedInput"]["answers"]["Which DB?"], "SQLite");
        assert_eq!(r["updatedInput"]["answers"]["Extras?"], "Tests, lint");
        assert_eq!(r["updatedInput"]["questions"][0]["header"], "DB");
    }

    #[test]
    fn exit_plan_mode_opens_a_plan() {
        let open = |t: &mut Translator| {
            t.on_line(
                r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","tool_use_id":"tu2","input":{"plan":"1. Do it"}}}"#,
            )
        };
        let mut t = Translator::new();
        let out = open(&mut t);
        let [HarnessEvent::RequestOpened { kind, detail, .. }] = events(&out).as_slice() else {
            panic!("expected a request: {out:?}");
        };
        assert_eq!(*kind, RequestKind::Plan);
        assert_eq!(detail["plan"], "1. Do it");
        // Approve and accept edits: leaves plan mode for acceptEdits.
        let v = sent(&t.respond("p1", DecisionKind::AllowSession, None));
        let r = &v["response"]["response"];
        assert_eq!(r["behavior"], "allow");
        assert_eq!(r["updatedPermissions"][0]["mode"], "acceptEdits");
        // Keep planning: the feedback goes back with the denial.
        open(&mut t);
        let reply = RequestReply {
            answers: vec![],
            message: Some("Use Postgres".into()),
        };
        let v = sent(&t.respond("p1", DecisionKind::Deny, Some(reply)));
        let r = &v["response"]["response"];
        assert_eq!(r["behavior"], "deny");
        assert!(r["message"].as_str().unwrap().contains("Use Postgres"));
        assert!(r.get("interrupt").is_none());
    }

    #[test]
    fn interrupt_marks_turn_interrupted() {
        let mut t = Translator::new();
        t.start_turn("x");
        assert_eq!(t.interrupt().len(), 1);
        let out =
            t.on_line(r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#);
        assert!(matches!(
            events(&out).last().unwrap(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None,
                ..
            }
        ));
    }

    #[test]
    fn unknown_control_request_gets_error_reply() {
        let mut t = Translator::new();
        let out = t.on_line(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"hook_callback"}}"#,
        );
        assert!(matches!(&out[0], Out::Send(v) if v["response"]["subtype"] == "error"));
    }

    #[test]
    fn tool_titles() {
        assert_eq!(tool_title("Bash", &json!({"command": "ls -la"})), "ls -la");
        assert_eq!(tool_title("Edit", &json!({"file_path": "/a.rs"})), "/a.rs");
        assert_eq!(tool_title("Weird", &json!({})), "Weird");
    }
}
