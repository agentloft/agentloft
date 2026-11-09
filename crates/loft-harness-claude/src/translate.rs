//! Pure translation of Claude Code stream-json into canonical harness events.
//!
//! No I/O lives here, so recorded CLI transcripts (`fixtures/`) can be replayed
//! in tests. Unknown message types are ignored (tolerant parsing, PLAN §2.10).

use loft_harness::{HarnessEvent, ToolResult};
use loft_protocol::{
    DecisionKind, DeltaChannel, ItemBody, ItemStatus, NoticeLevel, PermissionMode, RequestKind,
    ToolKind, TurnStatus, Usage,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// What the translator wants done: emit an event, or write a line to stdin.
#[derive(Debug, Clone)]
pub enum Out {
    Event(HarnessEvent),
    Send(Value),
}

#[derive(Debug, Clone)]
struct PendingApproval {
    input: Value,
    suggestions: Vec<Value>,
    tool_use_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Translator {
    /// (api message id, block index) → (native item id, channel).
    open_blocks: HashMap<(String, u64), (String, DeltaChannel)>,
    /// Message ids whose text/thinking was streamed via `stream_event`.
    streamed_messages: HashSet<String>,
    /// Tool calls started but without a result yet.
    open_tools: HashSet<String>,
    /// Tool calls the user denied (their error result becomes `Declined`).
    denied_tools: HashSet<String>,
    pending: HashMap<String, PendingApproval>,
    interrupt_requested: bool,
    turn_active: bool,
    next_request: u64,
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

    pub fn start_turn(&mut self, text: &str) -> Vec<Out> {
        self.turn_active = true;
        self.interrupt_requested = false;
        vec![
            Out::Send(json!({
                "type": "user",
                "message": {"role": "user", "content": text},
            })),
            Out::Event(HarnessEvent::TurnStarted),
        ]
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

    pub fn respond(&mut self, request_id: &str, decision: DecisionKind) -> Vec<Out> {
        let Some(pending) = self.pending.remove(request_id) else {
            return vec![];
        };
        let response = match decision {
            DecisionKind::AllowOnce => json!({"behavior": "allow", "updatedInput": pending.input}),
            DecisionKind::AllowSession => {
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
            DecisionKind::Deny => {
                json!({"behavior": "deny", "message": "The user denied this action."})
            }
            DecisionKind::DenyAndStop => {
                self.interrupt_requested = true;
                json!({"behavior": "deny", "message": "The user denied this action and stopped the turn.", "interrupt": true})
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
            Some("stream_event") => self.on_stream_event(v),
            Some("assistant") => self.on_assistant(v),
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
            Some("api_retry") => vec![Out::Event(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: "Claude API error — retrying…".into(),
            })],
            _ => vec![],
        }
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
                let Some((native_id, channel)) = self.open_blocks.get(&(msg_id, index)) else {
                    return vec![];
                };
                let delta = event.get("delta").unwrap_or(&Value::Null);
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
                    if !self.open_tools.insert(id.to_owned()) {
                        continue;
                    }
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
            if !self.open_tools.remove(tool_id) {
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
                        input: input.clone(),
                        suggestions: request
                            .get("permission_suggestions")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                        tool_use_id: tool_use_id.clone(),
                    },
                );
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
        // Anything still open is finished now.
        for ((_, _), (native_id, _)) in self.open_blocks.drain() {
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id,
                body: None,
                tool_result: None,
                status: ItemStatus::Completed,
            }));
        }
        for tool in self.open_tools.drain() {
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
        let usage = v.get("usage").map(|u| Usage {
            input_tokens: u64_at(u, "input_tokens"),
            output_tokens: u64_at(u, "output_tokens"),
            cache_read_tokens: u64_at(u, "cache_read_input_tokens"),
            cache_write_tokens: u64_at(u, "cache_creation_input_tokens"),
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
            duration_ms: v.get("duration_ms").and_then(Value::as_u64),
        });
        self.turn_active = false;
        self.interrupt_requested = false;
        self.streamed_messages.clear();
        self.denied_tools.clear();
        out.push(Out::Event(HarnessEvent::TurnCompleted {
            status,
            usage,
            error,
        }));
        out
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
                assert!(usage.as_ref().unwrap().cost_usd.is_some());
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
                let reply = t.respond(&id, DecisionKind::AllowSession);
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
