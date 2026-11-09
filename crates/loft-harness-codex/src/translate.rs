//! Pure translation between `codex app-server` JSON-RPC (JSONL, no
//! `jsonrpc` field) and canonical harness events. No I/O, so recorded
//! sessions in `fixtures/` replay in tests.

use loft_harness::{HarnessEvent, ToolResult};
use loft_protocol::{
    DecisionKind, DeltaChannel, ItemBody, ItemStatus, NoticeLevel, PermissionMode, RequestKind,
    ToolKind, TurnStatus, Usage,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone)]
pub enum Out {
    Event(HarnessEvent),
    Send(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    ThreadStart,
    ThreadResume,
    ThreadFork,
    TurnStart,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalKind {
    /// `item/commandExecution|fileChange/requestApproval` → `{decision}`.
    Decision,
    /// MCP tool call via `mcpServer/elicitation/request` → `{action}`.
    McpTool,
}

#[derive(Debug, Clone)]
struct OpenApproval {
    kind: ApprovalKind,
    /// The server's JSON-RPC id, echoed back verbatim.
    rpc_id: Value,
    available: Vec<Value>,
    command: Option<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct Translator {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Session settings applied at thread start / per turn.
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Option<PermissionMode>,
    resume: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// Prompts queued until the thread exists.
    queued: VecDeque<String>,
    approvals: HashMap<String, OpenApproval>,
    usage: Option<Usage>,
    interrupted: bool,
    /// MCP servers whose tool calls are approved automatically.
    pub auto_allow_mcp: Vec<String>,
    /// Open the session as a native fork of this Codex thread.
    pub fork_from: Option<String>,
}

impl Translator {
    pub fn new(
        cwd: String,
        model: Option<String>,
        mode: Option<PermissionMode>,
        resume: Option<String>,
    ) -> Self {
        Self {
            cwd,
            model,
            mode,
            resume,
            ..Self::default()
        }
    }

    fn request(&mut self, kind: Pending, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        json!({ "id": self.next_id, "method": method, "params": params })
    }

    // -- outgoing -----------------------------------------------------------

    pub fn initialize(&mut self) -> Value {
        self.request(
            Pending::Initialize,
            "initialize",
            json!({
                "clientInfo": { "name": "agentloft", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true },
            }),
        )
    }

    fn open_thread(&mut self) -> Value {
        match (self.resume.clone(), self.fork_from.clone()) {
            (Some(id), _) => self.request(
                Pending::ThreadResume,
                "thread/resume",
                json!({ "threadId": id }),
            ),
            (None, Some(parent)) => {
                // Native fork (ADR-0011), into this thread's own cwd.
                let mut params = json!({ "threadId": parent, "cwd": self.cwd });
                if let Some(model) = &self.model {
                    params["model"] = json!(model);
                }
                if let Some(mode) = self.mode {
                    let (approval, sandbox) = thread_policy(mode);
                    params["approvalPolicy"] = json!(approval);
                    params["sandbox"] = json!(sandbox);
                }
                self.request(Pending::ThreadFork, "thread/fork", params)
            }
            (None, None) => {
                let mut params = json!({ "cwd": self.cwd });
                if let Some(model) = &self.model {
                    params["model"] = json!(model);
                }
                if let Some(mode) = self.mode {
                    let (approval, sandbox) = thread_policy(mode);
                    params["approvalPolicy"] = json!(approval);
                    params["sandbox"] = json!(sandbox);
                }
                self.request(Pending::ThreadStart, "thread/start", params)
            }
        }
    }

    fn turn_start(&mut self, text: &str, thread: &str) -> Value {
        let mut params = json!({
            "threadId": thread,
            "input": [{ "type": "text", "text": text }],
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(mode) = self.mode {
            let (approval, sandbox) = turn_policy(mode);
            params["approvalPolicy"] = approval;
            params["sandboxPolicy"] = sandbox;
        }
        self.request(Pending::TurnStart, "turn/start", params)
    }

    pub fn start_turn(&mut self, text: &str) -> Vec<Out> {
        self.interrupted = false;
        self.usage = None;
        let mut out = vec![Out::Event(HarnessEvent::TurnStarted)];
        match self.thread_id.clone() {
            Some(thread) => out.push(Out::Send(self.turn_start(text, &thread))),
            None => self.queued.push_back(text.to_owned()),
        }
        out
    }

    pub fn interrupt(&mut self) -> Vec<Out> {
        let (Some(thread), Some(turn)) = (self.thread_id.clone(), self.turn_id.clone()) else {
            return vec![];
        };
        self.interrupted = true;
        vec![Out::Send(self.request(
            Pending::Other,
            "turn/interrupt",
            json!({ "threadId": thread, "turnId": turn }),
        ))]
    }

    pub fn set_model(&mut self, model: Option<String>) -> Vec<Out> {
        self.model = model; // applied on the next turn/start
        vec![]
    }

    pub fn set_permission_mode(&mut self, mode: Option<PermissionMode>) -> Vec<Out> {
        self.mode = mode;
        vec![]
    }

    pub fn respond(&mut self, request_id: &str, decision: DecisionKind) -> Vec<Out> {
        let Some(open) = self.approvals.remove(request_id) else {
            return vec![];
        };
        if decision == DecisionKind::DenyAndStop {
            self.interrupted = true;
        }
        let result = match open.kind {
            ApprovalKind::Decision => json!({
                "decision": decision_value(decision, &open.available, open.command.as_deref())
            }),
            ApprovalKind::McpTool => mcp_action(decision),
        };
        vec![
            Out::Send(json!({ "id": open.rpc_id, "result": result })),
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
                tracing::warn!(%err, "unparseable codex line");
                vec![]
            }
        }
    }

    pub fn on_value(&mut self, v: &Value) -> Vec<Out> {
        let method = v.get("method").and_then(Value::as_str);
        match (method, v.get("id")) {
            (Some(m), Some(id)) => {
                self.on_server_request(m, id.clone(), v.get("params").unwrap_or(&Value::Null))
            }
            (Some(m), None) => self.on_notification(m, v.get("params").unwrap_or(&Value::Null)),
            (None, Some(id)) => self.on_response(id, v),
            (None, None) => vec![],
        }
    }

    fn on_response(&mut self, id: &Value, v: &Value) -> Vec<Out> {
        let Some(kind) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
            return vec![];
        };
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
                .to_owned();
            return match kind {
                // A lost/unknown thread: start fresh rather than failing forever.
                Pending::ThreadResume => {
                    self.resume = None;
                    vec![
                        Out::Event(HarnessEvent::Notice {
                            level: NoticeLevel::Warning,
                            text: format!(
                                "Couldn't resume the Codex thread ({msg}); started a new one."
                            ),
                        }),
                        Out::Send(self.open_thread()),
                    ]
                }
                Pending::ThreadFork => {
                    self.fork_from = None;
                    vec![
                        Out::Event(HarnessEvent::Notice {
                            level: NoticeLevel::Warning,
                            text: format!(
                                "Couldn't fork the Codex thread ({msg}); started a new one."
                            ),
                        }),
                        Out::Send(self.open_thread()),
                    ]
                }
                Pending::Initialize | Pending::ThreadStart | Pending::TurnStart => {
                    vec![Out::Event(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        usage: None,
                        error: Some(format!("Codex: {msg}")),
                    })]
                }
                Pending::Other => vec![Out::Event(HarnessEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("Codex: {msg}"),
                })],
            };
        }
        let result = v.get("result").unwrap_or(&Value::Null);
        match kind {
            Pending::Initialize => {
                let mut out = vec![Out::Send(json!({ "method": "initialized" }))];
                out.push(Out::Send(self.open_thread()));
                out
            }
            Pending::ThreadStart | Pending::ThreadResume | Pending::ThreadFork => {
                let thread = result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.thread_id = Some(thread.clone());
                let mut out = vec![Out::Event(HarnessEvent::SessionReady {
                    native_session_id: thread.clone(),
                    model: result
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    slash_commands: vec![],
                })];
                while let Some(text) = self.queued.pop_front() {
                    out.push(Out::Send(self.turn_start(&text, &thread)));
                }
                out
            }
            Pending::TurnStart => {
                if let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_owned());
                }
                vec![]
            }
            Pending::Other => vec![],
        }
    }

    fn on_notification(&mut self, method: &str, p: &Value) -> Vec<Out> {
        match method {
            "turn/started" => {
                if let Some(turn) = p.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_owned());
                }
                vec![]
            }
            "item/started" => item_started(p.get("item").unwrap_or(&Value::Null), &self.cwd)
                .map(|e| vec![Out::Event(e)])
                .unwrap_or_default(),
            "item/completed" => item_completed(p.get("item").unwrap_or(&Value::Null), &self.cwd)
                .map(|e| vec![Out::Event(e)])
                .unwrap_or_default(),
            "item/agentMessage/delta" => delta(p, DeltaChannel::Text),
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                delta(p, DeltaChannel::Reasoning)
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                delta(p, DeltaChannel::Output)
            }
            "thread/tokenUsage/updated" => {
                if let Some(last) = p.pointer("/tokenUsage/last") {
                    let n = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let cached = n("cachedInputTokens");
                    self.usage = Some(Usage {
                        input_tokens: n("inputTokens").saturating_sub(cached),
                        output_tokens: n("outputTokens"),
                        cache_read_tokens: cached,
                        cache_write_tokens: n("cacheWriteInputTokens"),
                        cost_usd: None,
                        duration_ms: None,
                    });
                }
                vec![]
            }
            "serverRequest/resolved" => {
                let id = p.get("requestId").map(id_string).unwrap_or_default();
                match self.approvals.remove(&id) {
                    Some(_) => vec![Out::Event(HarnessEvent::RequestResolved { native_id: id })],
                    None => vec![],
                }
            }
            "turn/completed" => {
                let turn = p.get("turn").unwrap_or(&Value::Null);
                let status = match turn.get("status").and_then(Value::as_str) {
                    Some("completed") => TurnStatus::Completed,
                    Some("interrupted") => TurnStatus::Interrupted,
                    _ if self.interrupted => TurnStatus::Interrupted,
                    _ => TurnStatus::Failed,
                };
                let error = (status == TurnStatus::Failed).then(|| {
                    turn.pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex turn failed")
                        .to_owned()
                });
                let mut usage = self.usage.take();
                if let Some(u) = usage.as_mut() {
                    u.duration_ms = turn.get("durationMs").and_then(Value::as_u64);
                }
                let mut out: Vec<Out> = self
                    .approvals
                    .drain()
                    .map(|(id, _)| Out::Event(HarnessEvent::RequestResolved { native_id: id }))
                    .collect();
                self.turn_id = None;
                self.interrupted = false;
                out.push(Out::Event(HarnessEvent::TurnCompleted {
                    status,
                    usage,
                    error,
                }));
                out
            }
            "error" => {
                let msg = p
                    .pointer("/error/message")
                    .or_else(|| p.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex reported an error");
                vec![Out::Event(HarnessEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: msg.to_owned(),
                })]
            }
            _ => vec![],
        }
    }

    fn on_server_request(&mut self, method: &str, id: Value, p: &Value) -> Vec<Out> {
        let native_id = id_string(&id);
        let item_id = p.get("itemId").and_then(Value::as_str).map(str::to_owned);
        let available: Vec<Value> = p
            .get("availableDecisions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let reason = p.get("reason").and_then(Value::as_str).map(str::to_owned);
        let (kind, title, detail, command) = match method {
            "item/commandExecution/requestApproval" => {
                let cmd = display_command(p);
                let amendment = p
                    .get("proposedExecpolicyAmendment")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect()
                    });
                (
                    RequestKind::ToolApproval,
                    format!("Shell: {cmd}"),
                    json!({ "tool": "Shell", "input": { "command": cmd }, "decision_reason": reason }),
                    amendment,
                )
            }
            "item/fileChange/requestApproval" => (
                RequestKind::FileChangeApproval,
                "Edit: apply file changes".to_owned(),
                json!({ "tool": "Edit", "input": {}, "decision_reason": reason }),
                None,
            ),
            "mcpServer/elicitation/request"
                if p.pointer("/_meta/codex_approval_kind")
                    .and_then(Value::as_str)
                    == Some("mcp_tool_call") =>
            {
                let server = p.get("serverName").and_then(Value::as_str).unwrap_or("mcp");
                if self.auto_allow_mcp.iter().any(|s| s == server) {
                    return vec![Out::Send(
                        json!({ "id": id, "result": mcp_action(DecisionKind::AllowSession) }),
                    )];
                }
                let tool = p
                    .pointer("/_meta/tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let params = p
                    .pointer("/_meta/tool_params")
                    .cloned()
                    .unwrap_or(json!({}));
                let open = OpenApproval {
                    kind: ApprovalKind::McpTool,
                    rpc_id: id,
                    available: vec![],
                    command: None,
                };
                self.approvals.insert(native_id.clone(), open);
                return vec![Out::Event(HarnessEvent::RequestOpened {
                    native_id,
                    kind: RequestKind::ToolApproval,
                    title: format!("MCP: {server} · {tool}"),
                    detail: json!({ "tool": format!("mcp__{server}__{tool}"), "input": params,
                                    "decision_reason": p.get("message") }),
                    options: vec![
                        DecisionKind::AllowOnce,
                        DecisionKind::AllowSession,
                        DecisionKind::Deny,
                        DecisionKind::DenyAndStop,
                    ],
                    related_item: None,
                })];
            }
            "mcpServer/elicitation/request" => {
                // Real MCP form elicitations aren't supported yet: decline.
                return vec![Out::Send(
                    json!({ "id": id, "result": { "action": "decline" } }),
                )];
            }
            _ => {
                // Questions, MCP elicitations, auth refresh… aren't supported yet;
                // answer so Codex never blocks waiting.
                return vec![Out::Send(json!({
                    "id": id,
                    "error": { "code": -32601, "message": format!("Agentloft does not handle {method} yet") }
                }))];
            }
        };
        let options = decision_options(&available);
        self.approvals.insert(
            native_id.clone(),
            OpenApproval {
                kind: ApprovalKind::Decision,
                rpc_id: id,
                available,
                command,
            },
        );
        vec![Out::Event(HarnessEvent::RequestOpened {
            native_id,
            kind,
            title,
            detail,
            options,
            related_item: item_id,
        })]
    }
}

fn id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn delta(p: &Value, channel: DeltaChannel) -> Vec<Out> {
    let (Some(item), Some(text)) = (
        p.get("itemId").and_then(Value::as_str),
        p.get("delta").and_then(Value::as_str),
    ) else {
        return vec![];
    };
    if text.is_empty() {
        return vec![];
    }
    vec![Out::Event(HarnessEvent::ItemDelta {
        native_id: item.to_owned(),
        channel,
        text: text.to_owned(),
    })]
}

/// `/bin/zsh -lc 'touch x'` → `touch x` (prefer Codex's parsed actions).
fn display_command(item: &Value) -> String {
    let actions: Vec<&str> = item
        .get("commandActions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("command").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !actions.is_empty() {
        return actions.join(" && ");
    }
    let raw = item
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for prefix in ["/bin/zsh -lc ", "/bin/bash -lc ", "bash -lc ", "zsh -lc "] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return rest.trim_matches('\'').to_owned();
        }
    }
    raw.to_owned()
}

fn change_paths(item: &Value, cwd: &str) -> Vec<String> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("path").and_then(Value::as_str))
                .map(|p| {
                    p.strip_prefix(&format!("{}/", cwd.trim_end_matches('/')))
                        .unwrap_or(p)
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn tool_body(item: &Value, cwd: &str) -> Option<ItemBody> {
    let tool = |tool: &str, kind: ToolKind, title: String, input: Value| ItemBody::ToolCall {
        tool: tool.to_owned(),
        tool_kind: kind,
        title,
        input,
        output: None,
        is_error: false,
    };
    Some(match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => {
            let cmd = display_command(item);
            tool(
                "Shell",
                ToolKind::Command,
                cmd.clone(),
                json!({ "command": cmd }),
            )
        }
        "fileChange" => {
            let paths = change_paths(item, cwd);
            let first = paths.first().cloned().unwrap_or_default();
            let title = match paths.len() {
                0 => "files".to_owned(),
                1 => first.clone(),
                n => format!("{first} +{} more", n - 1),
            };
            tool(
                "Edit",
                ToolKind::FileChange,
                title,
                json!({ "file_path": first, "paths": paths }),
            )
        }
        "mcpToolCall" => {
            let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
            let name = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let full = format!("mcp__{server}__{name}");
            tool(
                &full,
                ToolKind::Mcp,
                full.clone(),
                item.get("arguments").cloned().unwrap_or(json!({})),
            )
        }
        "dynamicToolCall" => {
            let name = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            tool(
                name,
                ToolKind::Other,
                name.to_owned(),
                item.get("arguments").cloned().unwrap_or(json!({})),
            )
        }
        "webSearch" => {
            let q = item
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            tool(
                "WebSearch",
                ToolKind::Search,
                q.to_owned(),
                json!({ "query": q }),
            )
        }
        "collabAgentToolCall" => {
            let prompt = item
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let title: String = prompt
                .lines()
                .next()
                .unwrap_or("Subagent")
                .chars()
                .take(80)
                .collect();
            tool(
                "Agent",
                ToolKind::Subagent,
                title,
                json!({ "prompt": prompt, "description": prompt }),
            )
        }
        "imageView" => {
            let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
            tool(
                "ViewImage",
                ToolKind::FileRead,
                path.to_owned(),
                json!({ "file_path": path }),
            )
        }
        _ => return None,
    })
}

fn item_started(item: &Value, cwd: &str) -> Option<HarnessEvent> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let body = match item.get("type").and_then(Value::as_str)? {
        "agentMessage" => ItemBody::AssistantMessage {
            text: String::new(),
        },
        "reasoning" | "plan" => ItemBody::Reasoning {
            text: String::new(),
        },
        "contextCompaction" => ItemBody::Notice {
            level: NoticeLevel::Info,
            text: "Context compacted".into(),
        },
        _ => tool_body(item, cwd)?,
    };
    Some(HarnessEvent::ItemStarted {
        native_id: id,
        body,
        parent: None,
    })
}

fn item_completed(item: &Value, cwd: &str) -> Option<HarnessEvent> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let kind = item.get("type").and_then(Value::as_str)?;
    let status_str = item.get("status").and_then(Value::as_str);
    let status = match status_str {
        Some("failed") => ItemStatus::Failed,
        Some("declined") => ItemStatus::Declined,
        _ => ItemStatus::Completed,
    };
    let (body, tool_result) = match kind {
        "agentMessage" => (
            Some(ItemBody::AssistantMessage {
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            None,
        ),
        "reasoning" => {
            let parts = |k: &str| {
                item.get(k)
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default()
            };
            let summary = parts("summary");
            let text = if summary.is_empty() {
                parts("content")
            } else {
                summary
            };
            (Some(ItemBody::Reasoning { text }), None)
        }
        "plan" => (
            Some(ItemBody::Reasoning {
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            None,
        ),
        "commandExecution" => {
            let exit = item.get("exitCode").and_then(Value::as_i64);
            let output = item
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (
                None,
                Some(ToolResult {
                    output,
                    is_error: exit.is_some_and(|c| c != 0),
                }),
            )
        }
        "fileChange" => {
            let diff = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.get("diff").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (
                None,
                Some(ToolResult {
                    output: diff,
                    is_error: false,
                }),
            )
        }
        "mcpToolCall" => {
            let failed = item.get("error").is_some_and(|e| !e.is_null());
            let output = if failed {
                item.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_owned()
            } else {
                item.pointer("/result/content")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            };
            (
                None,
                Some(ToolResult {
                    output,
                    is_error: failed,
                }),
            )
        }
        "userMessage" | "hookPrompt" => return None,
        _ => {
            let _ = cwd;
            (None, None)
        }
    };
    let status = match (&tool_result, status) {
        (Some(r), ItemStatus::Completed) if r.is_error => ItemStatus::Failed,
        (_, s) => s,
    };
    Some(HarnessEvent::ItemCompleted {
        native_id: id,
        body,
        tool_result,
        status,
    })
}

fn mcp_action(decision: DecisionKind) -> Value {
    match decision {
        DecisionKind::AllowOnce => json!({ "action": "accept", "content": {} }),
        DecisionKind::AllowSession => {
            json!({ "action": "accept", "content": {}, "_meta": { "persist": "session" } })
        }
        DecisionKind::Deny => json!({ "action": "decline" }),
        DecisionKind::DenyAndStop => json!({ "action": "cancel" }),
    }
}

fn has(available: &[Value], name: &str) -> bool {
    available
        .iter()
        .any(|d| d.as_str() == Some(name) || d.get(name).is_some())
}

/// Which canonical options to offer, from Codex's `availableDecisions`.
fn decision_options(available: &[Value]) -> Vec<DecisionKind> {
    if available.is_empty() {
        return vec![
            DecisionKind::AllowOnce,
            DecisionKind::AllowSession,
            DecisionKind::Deny,
            DecisionKind::DenyAndStop,
        ];
    }
    let mut out = Vec::new();
    if has(available, "accept") {
        out.push(DecisionKind::AllowOnce);
    }
    if has(available, "acceptForSession") || has(available, "acceptWithExecpolicyAmendment") {
        out.push(DecisionKind::AllowSession);
    }
    if has(available, "decline") {
        out.push(DecisionKind::Deny);
    }
    if has(available, "cancel") {
        out.push(DecisionKind::DenyAndStop);
    }
    out
}

/// The `decision` value Codex expects for a canonical choice.
fn decision_value(
    decision: DecisionKind,
    available: &[Value],
    amendment: Option<&[String]>,
) -> Value {
    match decision {
        DecisionKind::AllowOnce => json!("accept"),
        DecisionKind::AllowSession => {
            if available.is_empty() || has(available, "acceptForSession") {
                json!("acceptForSession")
            } else if let Some(cmd) = amendment {
                json!({ "acceptWithExecpolicyAmendment": { "execpolicy_amendment": cmd } })
            } else {
                json!("accept")
            }
        }
        DecisionKind::Deny => {
            if available.is_empty() || has(available, "decline") {
                json!("decline")
            } else {
                json!("cancel")
            }
        }
        DecisionKind::DenyAndStop => json!("cancel"),
    }
}

/// thread/start: (approvalPolicy, sandbox mode).
fn thread_policy(mode: PermissionMode) -> (&'static str, &'static str) {
    match mode {
        PermissionMode::ReadOnly => ("untrusted", "read-only"),
        PermissionMode::AskAll => ("untrusted", "workspace-write"),
        PermissionMode::AcceptEdits | PermissionMode::Auto => ("on-request", "workspace-write"),
        PermissionMode::FullAccess => ("never", "danger-full-access"),
    }
}

/// turn/start: (approvalPolicy, sandboxPolicy).
fn turn_policy(mode: PermissionMode) -> (Value, Value) {
    let (approval, sandbox) = thread_policy(mode);
    let policy = match sandbox {
        "read-only" => json!({ "type": "readOnly" }),
        "danger-full-access" => json!({ "type": "dangerFullAccess" }),
        _ => json!({ "type": "workspaceWrite" }),
    };
    (json!(approval), policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(out: &[Out]) -> Vec<&HarnessEvent> {
        out.iter()
            .filter_map(|o| match o {
                Out::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fork_opens_thread_fork_and_falls_back_on_error() {
        let mut t = Translator::new("/tmp/wt".into(), None, None, None);
        t.fork_from = Some("parent-thread".into());
        t.initialize();
        let out = t.on_line(r#"{"id":1,"result":{}}"#);
        let Out::Send(fork) = &out[1] else { panic!() };
        assert_eq!(fork["method"], "thread/fork");
        assert_eq!(fork["params"]["threadId"], "parent-thread");
        assert_eq!(fork["params"]["cwd"], "/tmp/wt");
        // A failed fork starts a fresh thread instead, with a notice.
        let id = fork["id"].as_u64().unwrap();
        let out = t.on_line(&format!(
            r#"{{"id":{id},"error":{{"message":"no rollout"}}}}"#
        ));
        assert!(matches!(events(&out)[0], HarnessEvent::Notice { .. }));
        let Out::Send(start) = &out[1] else { panic!() };
        assert_eq!(start["method"], "thread/start");
        // The fork's own id becomes the session id.
        let id = start["id"].as_u64().unwrap();
        let out = t.on_line(&format!(
            r#"{{"id":{id},"result":{{"thread":{{"id":"child"}}}}}}"#
        ));
        assert!(matches!(
            events(&out)[0],
            HarnessEvent::SessionReady { native_session_id, .. } if native_session_id == "child"
        ));
    }

    #[test]
    fn handshake_then_thread_then_queued_turn() {
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        let init = t.initialize();
        assert_eq!(init["method"], "initialize");
        // The prompt arrives before the thread exists → queued.
        let out = t.start_turn("hi");
        assert!(matches!(events(&out)[0], HarnessEvent::TurnStarted));
        assert_eq!(out.len(), 1);
        let out = t.on_line(r#"{"id":1,"result":{}}"#);
        let Out::Send(initialized) = &out[0] else {
            panic!()
        };
        assert_eq!(initialized["method"], "initialized");
        let Out::Send(start) = &out[1] else { panic!() };
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"]["cwd"], "/tmp/probe");
        assert!(
            start["params"].get("approvalPolicy").is_none(),
            "harness default"
        );
        let out = t.on_line(r#"{"id":2,"result":{"thread":{"id":"T1"}}}"#);
        assert!(
            matches!(events(&out)[0], HarnessEvent::SessionReady { native_session_id, .. } if native_session_id == "T1")
        );
        let Out::Send(turn) = out.last().unwrap() else {
            panic!()
        };
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["input"][0]["text"], "hi");
    }

    #[test]
    fn recorded_turn_replays_with_approvals() {
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        t.thread_id = Some("01a1bf23-3d1c-7520-a69a-e0dcaad7294a".into());
        let mut out = t.start_turn("x");
        let mut approvals = 0;
        for line in include_str!("../fixtures/0.167.0-turn-with-approvals.jsonl").lines() {
            let produced = t.on_line(line);
            for o in &produced {
                if let Out::Event(HarnessEvent::RequestOpened {
                    native_id,
                    options,
                    title,
                    ..
                }) = o
                {
                    approvals += 1;
                    assert!(title.starts_with("Shell: "), "{title}");
                    // Codex offered accept / execpolicy amendment / cancel.
                    assert_eq!(
                        options,
                        &vec![
                            DecisionKind::AllowOnce,
                            DecisionKind::AllowSession,
                            DecisionKind::DenyAndStop
                        ]
                    );
                    let reply = t.respond(native_id, DecisionKind::AllowOnce);
                    let Out::Send(v) = &reply[0] else { panic!() };
                    assert_eq!(v["result"]["decision"], "accept");
                    out.extend(reply);
                }
            }
            out.extend(produced);
        }
        assert_eq!(approvals, 2);
        let evs = events(&out);
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemDelta {
                    channel: DeltaChannel::Text,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("a.txt"), "{text}");
        let shells = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ItemStarted { body: ItemBody::ToolCall { tool, .. }, .. } if tool == "Shell"))
            .count();
        assert_eq!(shells, 2);
        match evs.last().unwrap() {
            HarnessEvent::TurnCompleted { status, usage, .. } => {
                assert_eq!(*status, TurnStatus::Completed);
                let u = usage.as_ref().expect("usage");
                assert!(u.output_tokens > 0 && u.duration_ms.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decisions_follow_available_list() {
        let avail = vec![json!("accept"), json!("decline"), json!("cancel")];
        assert_eq!(
            decision_options(&avail),
            vec![
                DecisionKind::AllowOnce,
                DecisionKind::Deny,
                DecisionKind::DenyAndStop
            ]
        );
        assert_eq!(
            decision_value(DecisionKind::Deny, &avail, None),
            json!("decline")
        );
        let amend = vec![
            json!("accept"),
            json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["npm", "test"]}}),
        ];
        let cmd = vec!["npm".to_owned(), "test".to_owned()];
        assert_eq!(
            decision_value(DecisionKind::AllowSession, &amend, Some(&cmd)),
            json!({"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["npm", "test"]}})
        );
        assert_eq!(
            decision_value(DecisionKind::Deny, &amend, None),
            json!("cancel")
        );
    }

    #[test]
    fn interrupt_and_modes() {
        let mut t = Translator::new(
            "/w".into(),
            Some("gpt-x".into()),
            Some(PermissionMode::FullAccess),
            None,
        );
        t.thread_id = Some("T".into());
        let out = t.start_turn("go");
        let Out::Send(turn) = &out[1] else { panic!() };
        assert_eq!(turn["params"]["approvalPolicy"], "never");
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "dangerFullAccess");
        assert_eq!(turn["params"]["model"], "gpt-x");
        assert!(t.interrupt().is_empty(), "no turn id yet");
        t.on_line(r#"{"method":"turn/started","params":{"turn":{"id":"U"}}}"#);
        let out = t.interrupt();
        let Out::Send(v) = &out[0] else { panic!() };
        assert_eq!(v["method"], "turn/interrupt");
        let out = t.on_line(
            r#"{"method":"turn/completed","params":{"turn":{"id":"U","status":"interrupted"}}}"#,
        );
        assert!(matches!(
            events(&out).last().unwrap(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn mcp_tool_calls_follow_auto_allow() {
        let req = |server: &str| {
            format!(
                r#"{{"id":4,"method":"mcpServer/elicitation/request","params":{{"serverName":"{server}","mode":"form","_meta":{{"codex_approval_kind":"mcp_tool_call","tool_name":"list_threads","tool_params":{{}}}},"message":"Allow?"}}}}"#
            )
        };
        let mut t = Translator::new("/w".into(), None, None, None);
        t.auto_allow_mcp = vec!["agentloft".into()];
        let out = t.on_line(&req("agentloft"));
        let Out::Send(v) = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!(v["result"]["action"], "accept");

        let out = t.on_line(&req("github"));
        let Out::Event(HarnessEvent::RequestOpened {
            native_id, title, ..
        }) = &out[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(title, "MCP: github · list_threads");
        let reply = t.respond(native_id, DecisionKind::Deny);
        let Out::Send(v) = &reply[0] else { panic!() };
        assert_eq!(v["id"], 4);
        assert_eq!(v["result"]["action"], "decline");
    }

    #[test]
    fn unsupported_server_requests_are_answered() {
        let mut t = Translator::new("/w".into(), None, None, None);
        let out = t.on_line(r#"{"id":7,"method":"item/tool/requestUserInput","params":{}}"#);
        let Out::Send(v) = &out[0] else { panic!() };
        assert_eq!(v["id"], 7);
        assert!(v.get("error").is_some());
    }
}
