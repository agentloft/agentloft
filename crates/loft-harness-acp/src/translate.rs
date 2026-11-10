//! Pure translation between ACP (Agent Client Protocol v1, JSON-RPC 2.0 over
//! newline-delimited stdio) and canonical harness events. No I/O, so recorded
//! sessions in `fixtures/` replay in tests.
//!
//! Field names follow the stable v1 schema (`agent-client-protocol-schema`
//! 1.15) and were checked against OpenCode 2.2.5's real output. Parsing is
//! tolerant: unknown updates, fields and content blocks are ignored.

use loft_harness::{HarnessEvent, McpServer, ToolResult};
use loft_protocol::{
    DecisionKind, DeltaChannel, ItemBody, ItemStatus, NoticeLevel, PermissionMode, RequestKind,
    ToolKind, TurnStatus, Usage,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};

/// ACP protocol version we speak.
pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug, Clone)]
pub enum Out {
    Event(HarnessEvent),
    Send(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    LoadSession,
    ResumeSession,
    Prompt,
    /// `session/set_config_option`, `session/set_mode`, `session/set_model`.
    Setting,
}

/// A select-style session setting (model or mode) the agent advertised.
#[derive(Debug, Clone, PartialEq)]
enum Setting {
    /// `configOptions[]` entry → `session/set_config_option {configId, value}`.
    ConfigOption {
        id: String,
        current: String,
        values: Vec<String>,
    },
    /// Legacy `modes` → `session/set_mode {modeId}`.
    LegacyMode {
        current: String,
        values: Vec<String>,
    },
    /// Legacy (unstable) `models` → `session/set_model {modelId}`.
    LegacyModel {
        current: String,
        values: Vec<String>,
    },
}

impl Setting {
    fn current(&self) -> &str {
        match self {
            Setting::ConfigOption { current, .. }
            | Setting::LegacyMode { current, .. }
            | Setting::LegacyModel { current, .. } => current,
        }
    }

    fn values(&self) -> &[String] {
        match self {
            Setting::ConfigOption { values, .. }
            | Setting::LegacyMode { values, .. }
            | Setting::LegacyModel { values, .. } => values,
        }
    }

    fn set_current(&mut self, value: &str) {
        match self {
            Setting::ConfigOption { current, .. }
            | Setting::LegacyMode { current, .. }
            | Setting::LegacyModel { current, .. } => *current = value.to_owned(),
        }
    }
}

/// One `tool_call`, merged with every `tool_call_update` seen so far.
#[derive(Debug, Clone, Default)]
struct ToolState {
    /// `ItemStarted` was emitted.
    started: bool,
    /// `ItemCompleted` was emitted.
    done: bool,
    name: Option<String>,
    title: String,
    kind: Option<String>,
    status: Option<String>,
    raw_input: Value,
    raw_output: Value,
    content: Vec<Value>,
    locations: Vec<Value>,
    /// Output already streamed as `ItemDelta`s.
    streamed: String,
}

impl ToolState {
    /// Apply a `ToolCall` / `ToolCallUpdate` object: present fields replace.
    fn merge(&mut self, u: &Value) {
        let s = |k: &str| u.get(k).and_then(Value::as_str).map(str::to_owned);
        if let Some(v) = s("name") {
            self.name = Some(v);
        }
        if let Some(v) = s("title").filter(|t| !t.is_empty()) {
            self.title = v;
        }
        if let Some(v) = s("kind") {
            self.kind = Some(v);
        }
        if let Some(v) = s("status") {
            self.status = Some(v);
        }
        if let Some(v) = u.get("rawInput").filter(|v| !is_empty(v)) {
            self.raw_input = v.clone();
        }
        if let Some(v) = u.get("rawOutput").filter(|v| !v.is_null()) {
            self.raw_output = v.clone();
        }
        if let Some(v) = u.get("content").and_then(Value::as_array) {
            self.content = v.clone();
        }
        if let Some(v) = u.get("locations").and_then(Value::as_array) {
            self.locations = v.clone();
        }
    }

    fn is_finished(&self) -> bool {
        matches!(self.status.as_deref(), Some("completed" | "failed"))
    }

    /// Still waiting for input or approval; not worth showing yet.
    fn is_pending(&self) -> bool {
        matches!(self.status.as_deref(), None | Some("pending"))
    }
}

#[derive(Debug, Clone)]
struct OpenApproval {
    /// The agent's JSON-RPC id, echoed back verbatim.
    rpc_id: Value,
    /// `(kind, optionId)` pairs, in the agent's order.
    options: Vec<(String, String)>,
}

/// The streamed message or thought currently open.
#[derive(Debug, Clone)]
struct OpenStream {
    native_id: String,
    channel: DeltaChannel,
    message_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Translator {
    /// Human name for messages ("OpenCode").
    pub agent: String,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    pub cwd: String,
    pub model: Option<String>,
    pub mode: Option<PermissionMode>,
    resume: Option<String>,
    mcp_servers: Vec<McpServer>,
    /// MCP servers whose tool calls are approved automatically.
    pub auto_allow_mcp: Vec<String>,

    // -- agent capabilities (from `initialize`) --
    load_session: bool,
    resume_session: bool,
    auth_hint: Option<String>,
    /// The session could not be opened; every turn fails with this.
    fatal: Option<String>,

    // -- session --
    session_id: Option<String>,
    /// `session/load` in flight: its history replay is not re-emitted.
    replaying: bool,
    model_setting: Option<Setting>,
    mode_setting: Option<Setting>,
    /// Setting changes in flight; prompts wait for them.
    settings_in_flight: usize,
    /// Prompts queued until the session exists, with `(mime, base64)` images.
    queued: VecDeque<(String, Vec<(String, String)>)>,
    /// The agent accepts image content blocks (`promptCapabilities.image`).
    images_ok: bool,

    // -- turn --
    prompting: bool,
    interrupted: bool,
    /// Cumulative session cost when the turn started (`usage_update.cost`).
    cost_at_start: Option<f64>,
    cost_now: Option<f64>,
    stream: Option<OpenStream>,
    seq: u64,
    tools: HashMap<String, ToolState>,
    /// Tool call ids in arrival order, for flushing at turn end.
    tool_order: Vec<String>,
    approvals: HashMap<String, OpenApproval>,
}

impl Translator {
    pub fn new(
        agent: impl Into<String>,
        cwd: String,
        model: Option<String>,
        mode: Option<PermissionMode>,
        resume: Option<String>,
        mcp_servers: Vec<McpServer>,
    ) -> Self {
        Self {
            agent: agent.into(),
            cwd,
            model,
            mode,
            resume,
            mcp_servers,
            ..Self::default()
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn request(&mut self, kind: Pending, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params })
    }

    fn notify(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    }

    fn reply(id: Value, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn notice(level: NoticeLevel, text: impl Into<String>) -> Out {
        Out::Event(HarnessEvent::Notice {
            level,
            text: text.into(),
        })
    }

    // -- outgoing -----------------------------------------------------------

    pub fn initialize(&mut self) -> Value {
        self.request(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                // The agent uses its own file system and shell tools.
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
                "clientInfo": { "name": "agentloft", "title": "Agentloft", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
    }

    fn mcp_servers_json(&self) -> Value {
        Value::Array(
            self.mcp_servers
                .iter()
                .map(|s| json!({ "name": s.name, "command": s.command, "args": s.args, "env": [] }))
                .collect(),
        )
    }

    fn open_session(&mut self) -> Vec<Out> {
        let mcp = self.mcp_servers_json();
        let cwd = self.cwd.clone();
        match self.resume.clone() {
            Some(id) if self.load_session => {
                self.replaying = true;
                vec![Out::Send(self.request(
                    Pending::LoadSession,
                    "session/load",
                    json!({ "sessionId": id, "cwd": cwd, "mcpServers": mcp }),
                ))]
            }
            Some(id) if self.resume_session => vec![Out::Send(self.request(
                Pending::ResumeSession,
                "session/resume",
                json!({ "sessionId": id, "cwd": cwd, "mcpServers": mcp }),
            ))],
            Some(_) => {
                self.resume = None;
                vec![
                    Self::notice(
                        NoticeLevel::Warning,
                        format!(
                            "{} can't resume sessions over ACP; started a new one.",
                            self.agent
                        ),
                    ),
                    Out::Send(self.request(
                        Pending::NewSession,
                        "session/new",
                        json!({ "cwd": cwd, "mcpServers": mcp }),
                    )),
                ]
            }
            None => vec![Out::Send(self.request(
                Pending::NewSession,
                "session/new",
                json!({ "cwd": cwd, "mcpServers": mcp }),
            ))],
        }
    }

    fn prompt(&mut self, text: &str, images: &[(String, String)], session: &str) -> Value {
        self.prompting = true;
        self.cost_at_start = self.cost_now;
        let mut prompt = vec![json!({ "type": "text", "text": text })];
        prompt.extend(
            images
                .iter()
                .map(|(mime, data)| json!({ "type": "image", "mimeType": mime, "data": data })),
        );
        self.request(
            Pending::Prompt,
            "session/prompt",
            json!({ "sessionId": session, "prompt": prompt }),
        )
    }

    fn ready_for_prompts(&self) -> Option<String> {
        (self.settings_in_flight == 0)
            .then(|| self.session_id.clone())
            .flatten()
    }

    fn flush_queue(&mut self) -> Vec<Out> {
        if self.prompting {
            return vec![];
        }
        let Some(session) = self.ready_for_prompts() else {
            return vec![];
        };
        // One prompt at a time; the core never queues more than one.
        match self.queued.pop_front() {
            Some((text, images)) => {
                let mut out = Vec::new();
                let images = if images.is_empty() || self.images_ok {
                    images
                } else {
                    out.push(Out::Event(HarnessEvent::Notice {
                        level: NoticeLevel::Warning,
                        text: "This agent doesn't accept images; they weren't sent.".into(),
                    }));
                    Vec::new()
                };
                out.push(Out::Send(self.prompt(&text, &images, &session)));
                out
            }
            None => vec![],
        }
    }

    pub fn start_turn(&mut self, text: &str) -> Vec<Out> {
        self.start_turn_with(text, Vec::new())
    }

    /// A prompt with images as `(mime, base64)`.
    pub fn start_turn_with(&mut self, text: &str, images: Vec<(String, String)>) -> Vec<Out> {
        self.interrupted = false;
        let mut out = vec![Out::Event(HarnessEvent::TurnStarted)];
        if let Some(error) = self.fatal.clone() {
            out.push(Out::Event(HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed,
                usage: None,
                error: Some(error),
            }));
            return out;
        }
        self.queued.push_back((text.to_owned(), images));
        out.extend(self.flush_queue());
        out
    }

    pub fn interrupt(&mut self) -> Vec<Out> {
        if !self.prompting {
            // Not sent yet (session still opening): drop it.
            if self.queued.pop_front().is_some() {
                return vec![Out::Event(HarnessEvent::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    usage: None,
                    error: None,
                })];
            }
            return vec![];
        }
        let Some(session) = self.session_id.clone() else {
            return vec![];
        };
        self.interrupted = true;
        let mut out = vec![Out::Send(Self::notify(
            "session/cancel",
            json!({ "sessionId": session }),
        ))];
        // The client MUST answer pending permission requests with `cancelled`.
        out.extend(self.cancel_approvals());
        out
    }

    fn cancel_approvals(&mut self) -> Vec<Out> {
        let mut ids: Vec<String> = self.approvals.keys().cloned().collect();
        ids.sort();
        let mut out = Vec::new();
        for id in ids {
            if let Some(open) = self.approvals.remove(&id) {
                out.push(Out::Send(Self::reply(
                    open.rpc_id,
                    json!({ "outcome": { "outcome": "cancelled" } }),
                )));
                out.push(Out::Event(HarnessEvent::RequestResolved { native_id: id }));
            }
        }
        out
    }

    pub fn set_model(&mut self, model: Option<String>) -> Vec<Out> {
        self.model = model;
        if self.session_id.is_none() {
            return vec![]; // applied once the session exists
        }
        self.apply_model(true)
    }

    pub fn set_permission_mode(&mut self, mode: Option<PermissionMode>) -> Vec<Out> {
        self.mode = mode;
        if self.session_id.is_none() {
            return vec![];
        }
        self.apply_mode(true)
    }

    fn setting_request(&mut self, setting: &Setting, value: &str) -> Value {
        let session = self.session_id.clone().unwrap_or_default();
        self.settings_in_flight += 1;
        match setting {
            Setting::ConfigOption { id, .. } => self.request(
                Pending::Setting,
                "session/set_config_option",
                json!({ "sessionId": session, "configId": id, "value": value }),
            ),
            Setting::LegacyMode { .. } => self.request(
                Pending::Setting,
                "session/set_mode",
                json!({ "sessionId": session, "modeId": value }),
            ),
            Setting::LegacyModel { .. } => self.request(
                Pending::Setting,
                "session/set_model",
                json!({ "sessionId": session, "modelId": value }),
            ),
        }
    }

    /// Ask the agent to switch to `self.model`, if it offers model selection.
    fn apply_model(&mut self, explicit: bool) -> Vec<Out> {
        let Some(wanted) = self.model.clone() else {
            return vec![];
        };
        let Some(mut setting) = self.model_setting.clone() else {
            return if explicit {
                vec![Self::notice(
                    NoticeLevel::Warning,
                    format!("{} doesn't offer model selection over ACP.", self.agent),
                )]
            } else {
                vec![]
            };
        };
        let Some(value) = resolve_model(&wanted, setting.values()) else {
            return vec![Self::notice(
                NoticeLevel::Warning,
                format!("{} doesn't offer the model `{wanted}`.", self.agent),
            )];
        };
        if setting.current() == value {
            return vec![];
        }
        let req = self.setting_request(&setting, &value);
        setting.set_current(&value);
        self.model_setting = Some(setting);
        vec![Out::Send(req)]
    }

    /// Ask the agent to switch to the mode matching `self.mode`.
    fn apply_mode(&mut self, explicit: bool) -> Vec<Out> {
        let Some(mode) = self.mode else {
            return vec![]; // harness default: leave the agent's mode alone
        };
        let Some(mut setting) = self.mode_setting.clone() else {
            return if explicit {
                vec![Self::notice(
                    NoticeLevel::Warning,
                    format!("{} doesn't offer permission modes over ACP.", self.agent),
                )]
            } else {
                vec![]
            };
        };
        let Some(value) = resolve_mode(mode, setting.values()) else {
            return vec![Self::notice(
                NoticeLevel::Warning,
                format!("{} has no mode matching {mode:?}.", self.agent),
            )];
        };
        if setting.current() == value {
            return vec![];
        }
        let req = self.setting_request(&setting, &value);
        setting.set_current(&value);
        self.mode_setting = Some(setting);
        vec![Out::Send(req)]
    }

    pub fn respond(&mut self, request_id: &str, decision: DecisionKind) -> Vec<Out> {
        let Some(open) = self.approvals.remove(request_id) else {
            return vec![];
        };
        let outcome = match pick_option(&open.options, decision) {
            Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
            None => json!({ "outcome": "cancelled" }),
        };
        let mut out = vec![
            Out::Send(Self::reply(open.rpc_id, json!({ "outcome": outcome }))),
            Out::Event(HarnessEvent::RequestResolved {
                native_id: request_id.to_owned(),
            }),
        ];
        if decision == DecisionKind::DenyAndStop {
            // PLAN §2.7: reject_once + session/cancel.
            out.extend(self.interrupt());
        }
        out
    }

    // -- incoming -----------------------------------------------------------

    pub fn on_line(&mut self, line: &str) -> Vec<Out> {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.on_value(&v),
            Err(err) => {
                tracing::warn!(%err, "unparseable ACP line");
                vec![]
            }
        }
    }

    pub fn on_value(&mut self, v: &Value) -> Vec<Out> {
        let method = v.get("method").and_then(Value::as_str);
        let params = v.get("params").unwrap_or(&Value::Null);
        match (method, v.get("id")) {
            (Some(m), Some(id)) => self.on_agent_request(m, id.clone(), params),
            (Some("session/update"), None) => self.on_session_update(params),
            (Some(_), None) => vec![],
            (None, Some(id)) => self.on_response(id, v),
            (None, None) => vec![],
        }
    }

    fn on_response(&mut self, id: &Value, v: &Value) -> Vec<Out> {
        let Some(kind) = id.as_u64().and_then(|id| self.pending.remove(&id)) else {
            return vec![];
        };
        if let Some(err) = v.get("error") {
            return self.on_error(kind, err);
        }
        let result = v.get("result").unwrap_or(&Value::Null);
        match kind {
            Pending::Initialize => {
                let caps = result.get("agentCapabilities").unwrap_or(&Value::Null);
                self.load_session = caps
                    .get("loadSession")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.images_ok = caps
                    .pointer("/promptCapabilities/image")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.resume_session = caps
                    .pointer("/sessionCapabilities/resume")
                    .is_some_and(|v| !v.is_null());
                self.auth_hint = result
                    .pointer("/authMethods/0/description")
                    .or_else(|| result.pointer("/authMethods/0/name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let name = result
                    .pointer("/agentInfo/name")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.agent);
                let version = result
                    .pointer("/agentInfo/version")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                tracing::info!(
                    agent = %name,
                    %version,
                    load_session = self.load_session,
                    resume = self.resume_session,
                    "ACP agent initialized"
                );
                self.open_session()
            }
            Pending::NewSession | Pending::LoadSession | Pending::ResumeSession => {
                self.replaying = false;
                let session = match kind {
                    Pending::NewSession => result
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    _ => self.resume.clone().unwrap_or_default(),
                };
                self.session_id = Some(session.clone());
                // A fresh session's cumulative cost starts at zero; a resumed
                // one's is unknown until the first update.
                self.cost_now = (kind == Pending::NewSession).then_some(0.0);
                self.read_settings(result);
                let mut out = vec![Out::Event(HarnessEvent::SessionReady {
                    models: offered_models(result),
                    native_session_id: session,
                    model: self
                        .model_setting
                        .as_ref()
                        .map(|s| s.current().to_owned())
                        .filter(|m| !m.is_empty()),
                    slash_commands: vec![],
                })];
                out.extend(self.apply_model(false));
                out.extend(self.apply_mode(false));
                out.extend(self.flush_queue());
                out
            }
            Pending::Prompt => self.finish_prompt(result),
            Pending::Setting => {
                self.settings_in_flight = self.settings_in_flight.saturating_sub(1);
                if result.get("configOptions").is_some() {
                    self.read_settings(result);
                }
                self.flush_queue()
            }
        }
    }

    fn on_error(&mut self, kind: Pending, err: &Value) -> Vec<Out> {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("request failed")
            .to_owned();
        let code = err.get("code").and_then(Value::as_i64);
        match kind {
            // A lost/unknown session: start fresh rather than failing forever.
            Pending::LoadSession | Pending::ResumeSession => {
                self.replaying = false;
                self.resume = None;
                let mut out = vec![Self::notice(
                    NoticeLevel::Warning,
                    format!(
                        "Couldn't resume the {} session ({msg}); started a new one.",
                        self.agent
                    ),
                )];
                out.extend(self.open_session());
                out
            }
            Pending::Initialize | Pending::NewSession => {
                let mut error = format!("{}: {msg}", self.agent);
                // -32000 = auth_required.
                if code == Some(-32000)
                    && let Some(hint) = &self.auth_hint
                {
                    error.push_str(&format!(" — {hint}"));
                }
                let had_turn = !self.queued.is_empty();
                self.queued.clear();
                self.fatal = Some(error.clone());
                let mut out = Vec::new();
                if had_turn {
                    out.push(Out::Event(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        usage: None,
                        error: Some(error),
                    }));
                } else {
                    out.push(Self::notice(NoticeLevel::Warning, error));
                }
                out
            }
            Pending::Prompt => {
                let (status, error) = if self.interrupted {
                    (TurnStatus::Interrupted, None)
                } else {
                    (TurnStatus::Failed, Some(format!("{}: {msg}", self.agent)))
                };
                self.end_turn(status, None, error)
            }
            Pending::Setting => {
                self.settings_in_flight = self.settings_in_flight.saturating_sub(1);
                let mut out = vec![Self::notice(
                    NoticeLevel::Warning,
                    format!("{}: {msg}", self.agent),
                )];
                out.extend(self.flush_queue());
                out
            }
        }
    }

    /// Track the agent's model/mode selectors (`configOptions`, or the legacy
    /// `modes` / `models` states).
    fn read_settings(&mut self, result: &Value) {
        if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
            for opt in options {
                let category = opt
                    .get("category")
                    .and_then(Value::as_str)
                    .or_else(|| opt.get("id").and_then(Value::as_str));
                let Some(setting) = config_select(opt) else {
                    continue;
                };
                match category {
                    Some("model") => self.model_setting = Some(setting),
                    Some("mode") => self.mode_setting = Some(setting),
                    _ => {}
                }
            }
        }
        let ids = |list: Option<&Value>, key: &str| -> Vec<String> {
            list.and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get(key).and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        if self.mode_setting.is_none()
            && let Some(modes) = result.get("modes").filter(|m| m.is_object())
        {
            self.mode_setting = Some(Setting::LegacyMode {
                current: str_at(modes, "currentModeId"),
                values: ids(modes.get("availableModes"), "id"),
            });
        }
        if self.model_setting.is_none()
            && let Some(models) = result.get("models").filter(|m| m.is_object())
        {
            self.model_setting = Some(Setting::LegacyModel {
                current: str_at(models, "currentModelId"),
                values: ids(models.get("availableModels"), "modelId"),
            });
        }
    }

    fn finish_prompt(&mut self, result: &Value) -> Vec<Out> {
        let stop = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        let (status, error) = match stop {
            _ if self.interrupted && stop != "end_turn" => (TurnStatus::Interrupted, None),
            "cancelled" => (TurnStatus::Interrupted, None),
            "refusal" => (
                TurnStatus::Failed,
                Some(format!("{} refused to continue.", self.agent)),
            ),
            "max_tokens" => (
                TurnStatus::Failed,
                Some(format!("{} stopped: it hit its token limit.", self.agent)),
            ),
            "max_turn_requests" => (
                TurnStatus::Failed,
                Some(format!(
                    "{} stopped: it hit its limit of model requests for one turn.",
                    self.agent
                )),
            ),
            // end_turn, and anything newer than us.
            _ => (TurnStatus::Completed, None),
        };
        let usage = result.get("usage").filter(|u| u.is_object()).map(|u| {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            Usage {
                input_tokens: n("inputTokens"),
                output_tokens: n("outputTokens"),
                cache_read_tokens: n("cachedReadTokens"),
                cache_write_tokens: n("cachedWriteTokens"),
                cost_usd: None,
                duration_ms: None,
            }
        });
        let cost = match (self.cost_at_start, self.cost_now) {
            (Some(a), Some(b)) if b >= a => Some(b - a),
            _ => None,
        };
        let usage = match (usage, cost) {
            (Some(mut u), cost) => {
                u.cost_usd = cost;
                Some(u)
            }
            (None, Some(cost)) => Some(Usage {
                cost_usd: Some(cost),
                ..Usage::default()
            }),
            (None, None) => None,
        };
        self.end_turn(status, usage, error)
    }

    fn end_turn(
        &mut self,
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<String>,
    ) -> Vec<Out> {
        let mut out = self.close_stream();
        // Tool calls the agent never ran (still pending) or never finished.
        let item_status = match status {
            TurnStatus::Completed | TurnStatus::Running => ItemStatus::Completed,
            TurnStatus::Interrupted => ItemStatus::Declined,
            TurnStatus::Failed => ItemStatus::Failed,
        };
        for id in std::mem::take(&mut self.tool_order) {
            let Some(mut tool) = self.tools.remove(&id) else {
                continue;
            };
            if tool.done {
                continue;
            }
            if !tool.started {
                out.extend(self.start_tool(&id, &mut tool));
            }
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id: id,
                body: Some(tool_body(&tool, &self.cwd, &self.mcp_names())),
                tool_result: None,
                status: item_status,
            }));
        }
        self.tools.clear();
        let mut ids: Vec<String> = self.approvals.keys().cloned().collect();
        ids.sort();
        for id in ids {
            self.approvals.remove(&id);
            out.push(Out::Event(HarnessEvent::RequestResolved { native_id: id }));
        }
        self.prompting = false;
        self.interrupted = false;
        out.push(Out::Event(HarnessEvent::TurnCompleted {
            status,
            usage,
            error,
        }));
        out.extend(self.flush_queue());
        out
    }

    fn mcp_names(&self) -> Vec<String> {
        self.mcp_servers.iter().map(|s| s.name.clone()).collect()
    }

    // -- session/update -----------------------------------------------------

    fn on_session_update(&mut self, p: &Value) -> Vec<Out> {
        if self.replaying {
            return vec![]; // history the thread already has
        }
        if let (Some(ours), Some(theirs)) = (
            self.session_id.as_deref(),
            p.get("sessionId").and_then(Value::as_str),
        ) && ours != theirs
        {
            return vec![]; // e.g. a subagent's child session
        }
        let u = p.get("update").unwrap_or(&Value::Null);
        match u.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => self.chunk(u, DeltaChannel::Text),
            Some("agent_thought_chunk") => self.chunk(u, DeltaChannel::Reasoning),
            Some("tool_call") | Some("tool_call_update") => self.tool_update(u),
            Some("plan") => self.plan(u),
            Some("current_mode_update") => {
                if let (Some(setting), Some(id)) = (
                    self.mode_setting.as_mut(),
                    u.get("currentModeId").and_then(Value::as_str),
                ) {
                    setting.set_current(id);
                }
                vec![]
            }
            Some("config_option_update") => {
                self.read_settings(u);
                vec![]
            }
            Some("usage_update") => {
                if u.pointer("/cost/currency").and_then(Value::as_str) == Some("USD") {
                    self.cost_now = u.pointer("/cost/amount").and_then(Value::as_f64);
                }
                vec![]
            }
            Some("notice") => {
                let level = match u.get("severity").and_then(Value::as_str) {
                    Some("info") => NoticeLevel::Info,
                    _ => NoticeLevel::Warning,
                };
                let title = str_at(u, "title");
                let text = match u.get("description").and_then(Value::as_str) {
                    Some(d) if !d.is_empty() => format!("{title}: {d}"),
                    _ => title,
                };
                vec![Self::notice(level, text)]
            }
            // user_message_chunk (replay only), available_commands_update,
            // session_info_update, compaction…: nothing to show.
            _ => vec![],
        }
    }

    fn close_stream(&mut self) -> Vec<Out> {
        match self.stream.take() {
            Some(s) => vec![Out::Event(HarnessEvent::ItemCompleted {
                native_id: s.native_id,
                body: None,
                tool_result: None,
                status: ItemStatus::Completed,
            })],
            None => vec![],
        }
    }

    fn chunk(&mut self, u: &Value, channel: DeltaChannel) -> Vec<Out> {
        let text = content_text(u.get("content").unwrap_or(&Value::Null));
        if text.is_empty() {
            return vec![];
        }
        let message_id = u
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut out = Vec::new();
        let same = self.stream.as_ref().is_some_and(|s| {
            s.channel == channel && (message_id.is_none() || s.message_id == message_id)
        });
        if !same {
            out.extend(self.close_stream());
            self.seq += 1;
            let (prefix, body) = match channel {
                DeltaChannel::Reasoning => (
                    "thought",
                    ItemBody::Reasoning {
                        text: String::new(),
                    },
                ),
                _ => (
                    "msg",
                    ItemBody::AssistantMessage {
                        text: String::new(),
                    },
                ),
            };
            let native_id = format!("acp:{prefix}:{}", self.seq);
            out.push(Out::Event(HarnessEvent::ItemStarted {
                native_id: native_id.clone(),
                body,
                parent: None,
            }));
            self.stream = Some(OpenStream {
                native_id,
                channel,
                message_id,
            });
        }
        if let Some(s) = &self.stream {
            out.push(Out::Event(HarnessEvent::ItemDelta {
                native_id: s.native_id.clone(),
                channel,
                text,
            }));
        }
        out
    }

    fn start_tool(&mut self, id: &str, tool: &mut ToolState) -> Vec<Out> {
        let mut out = self.close_stream();
        tool.started = true;
        out.push(Out::Event(HarnessEvent::ItemStarted {
            native_id: id.to_owned(),
            body: tool_body(tool, &self.cwd, &self.mcp_names()),
            parent: None,
        }));
        out
    }

    fn tool_update(&mut self, u: &Value) -> Vec<Out> {
        let Some(id) = u
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return vec![];
        };
        let mut tool = match self.tools.remove(&id) {
            Some(t) => t,
            None => {
                self.tool_order.push(id.clone());
                ToolState::default()
            }
        };
        tool.merge(u);
        let mut out = Vec::new();
        if tool.done {
            self.tools.insert(id, tool);
            return out;
        }
        // Agents announce calls as `pending` before their input is known;
        // show them once they run (or ask for approval).
        if !tool.started && (!tool.is_pending() || tool.is_finished()) {
            out.extend(self.start_tool(&id, &mut tool));
        }
        if tool.started && !tool.is_finished() {
            // Stream growing text output (content replaces on each update).
            let text = tool_output(&tool, &self.cwd);
            if let Some(suffix) = text.strip_prefix(tool.streamed.as_str())
                && !suffix.is_empty()
            {
                out.push(Out::Event(HarnessEvent::ItemDelta {
                    native_id: id.clone(),
                    channel: DeltaChannel::Output,
                    text: suffix.to_owned(),
                }));
                tool.streamed = text;
            }
        }
        if tool.started && tool.is_finished() {
            tool.done = true;
            let failed = tool.status.as_deref() == Some("failed");
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id: id.clone(),
                body: Some(tool_body(&tool, &self.cwd, &self.mcp_names())),
                tool_result: Some(ToolResult {
                    output: tool_output(&tool, &self.cwd),
                    is_error: failed,
                }),
                status: if failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                },
            }));
        }
        self.tools.insert(id, tool);
        out
    }

    fn plan(&mut self, u: &Value) -> Vec<Out> {
        let entries = u
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let todos: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "content": str_at(e, "content"),
                    "status": e.get("status").and_then(Value::as_str).unwrap_or("pending"),
                    "priority": e.get("priority").and_then(Value::as_str).unwrap_or("medium"),
                })
            })
            .collect();
        let done = todos.iter().filter(|t| t["status"] == "completed").count();
        let checklist = todos
            .iter()
            .map(|t| {
                let mark = match t["status"].as_str() {
                    Some("completed") => "[x]",
                    Some("in_progress") => "[~]",
                    _ => "[ ]",
                };
                format!("{mark} {}", t["content"].as_str().unwrap_or_default())
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = self.close_stream();
        self.seq += 1;
        let native_id = format!("acp:plan:{}", self.seq);
        out.push(Out::Event(HarnessEvent::ItemStarted {
            native_id: native_id.clone(),
            body: ItemBody::ToolCall {
                tool: "Plan".into(),
                tool_kind: ToolKind::Todo,
                title: format!("{done}/{} done", todos.len()),
                input: json!({ "todos": todos }),
                output: None,
                is_error: false,
            },
            parent: None,
        }));
        out.push(Out::Event(HarnessEvent::ItemCompleted {
            native_id,
            body: None,
            tool_result: Some(ToolResult {
                output: checklist,
                is_error: false,
            }),
            status: ItemStatus::Completed,
        }));
        out
    }

    // -- agent → client requests --------------------------------------------

    fn on_agent_request(&mut self, method: &str, id: Value, p: &Value) -> Vec<Out> {
        match method {
            "session/request_permission" => self.permission(id, p),
            _ => {
                // fs/* and terminal/* are not advertised; anything else is
                // newer than us. Answer so the agent never blocks.
                vec![Out::Send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("Agentloft does not handle {method}") },
                }))]
            }
        }
    }

    fn permission(&mut self, rpc_id: Value, p: &Value) -> Vec<Out> {
        let options: Vec<(String, String)> = p
            .get("options")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        Some((
                            o.get("kind")?.as_str()?.to_owned(),
                            o.get("optionId")?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let call = p.get("toolCall").unwrap_or(&Value::Null);
        let tool_id = call
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut out = Vec::new();
        let mut tool = tool_id
            .as_ref()
            .and_then(|id| self.tools.remove(id))
            .unwrap_or_default();
        let status = tool.status.clone();
        tool.merge(call);
        if !tool.started {
            // Asking for approval: show the call now.
            tool.status = Some("pending".into());
        } else {
            tool.status = status;
        }
        if let Some(id) = &tool_id
            && !self.tool_order.contains(id)
        {
            self.tool_order.push(id.clone());
        }
        let mcp_names = self.mcp_names();
        let body = tool_body(&tool, &self.cwd, &mcp_names);

        // Our own MCP server's tools (browser, thread reading) are pre-approved.
        if let ItemBody::ToolCall { tool: name, .. } = &body
            && let Some(server) = name
                .strip_prefix("mcp__")
                .and_then(|r| r.split("__").next())
            && self.auto_allow_mcp.iter().any(|s| s == server)
            && let Some(option_id) = pick_option(&options, DecisionKind::AllowOnce)
        {
            if let Some(id) = tool_id {
                self.tools.insert(id, tool);
            }
            return vec![Out::Send(Self::reply(
                rpc_id,
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
            ))];
        }

        if let Some(id) = &tool_id {
            if !tool.started {
                out.extend(self.start_tool(id, &mut tool));
            }
            self.tools.insert(id.clone(), tool);
        }

        let ItemBody::ToolCall {
            tool: name,
            tool_kind,
            title,
            input,
            ..
        } = body
        else {
            return out;
        };
        let native_id = id_string(&rpc_id);
        let request_title = match tool_kind {
            ToolKind::Command => format!(
                "Shell: {}",
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or(&title)
            ),
            ToolKind::FileChange => format!(
                "{name}: {}",
                input
                    .get("file_path")
                    .and_then(Value::as_str)
                    .map(|f| relative(f, &self.cwd))
                    .unwrap_or_else(|| title.clone())
            ),
            ToolKind::Mcp => {
                let rest = name.strip_prefix("mcp__").unwrap_or(&name);
                format!("MCP: {}", rest.replacen("__", " · ", 1))
            }
            _ => format!("{name}: {title}"),
        };
        let mut detail = json!({ "tool": name, "input": input, "decision_reason": Value::Null });
        let diff = diff_text(call.get("content"), &self.cwd);
        if !diff.is_empty() {
            detail["diff"] = json!(diff);
        }
        let mut canonical = Vec::new();
        let has = |k: &str| options.iter().any(|(kind, _)| kind == k);
        if has("allow_once") {
            canonical.push(DecisionKind::AllowOnce);
        }
        if has("allow_always") {
            canonical.push(DecisionKind::AllowSession);
        }
        if has("reject_once") || has("reject_always") {
            canonical.push(DecisionKind::Deny);
        }
        // Rejecting and cancelling the turn works with any agent.
        canonical.push(DecisionKind::DenyAndStop);
        if canonical.len() == 1 && options.is_empty() {
            canonical.insert(0, DecisionKind::AllowOnce);
        }
        self.approvals
            .insert(native_id.clone(), OpenApproval { rpc_id, options });
        out.push(Out::Event(HarnessEvent::RequestOpened {
            native_id,
            kind: if tool_kind == ToolKind::FileChange {
                RequestKind::FileChangeApproval
            } else {
                RequestKind::ToolApproval
            },
            title: request_title,
            detail,
            options: canonical,
            related_item: tool_id,
        }));
        out
    }
}

// -- helpers -----------------------------------------------------------------

fn id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// A `select` config option → setting (grouped or flat option lists).
fn config_select(opt: &Value) -> Option<Setting> {
    if opt.get("type").and_then(Value::as_str) != Some("select") {
        return None;
    }
    let mut values = Vec::new();
    for o in opt.get("options").and_then(Value::as_array)? {
        match o.get("options").and_then(Value::as_array) {
            Some(group) => values.extend(
                group
                    .iter()
                    .filter_map(|g| g.get("value").and_then(Value::as_str).map(str::to_owned)),
            ),
            None => values.extend(o.get("value").and_then(Value::as_str).map(str::to_owned)),
        }
    }
    Some(Setting::ConfigOption {
        id: opt.get("id").and_then(Value::as_str)?.to_owned(),
        current: str_at(opt, "currentValue"),
        values,
    })
}

/// Exact id, or `provider/<id>` when the user named a bare model.
fn resolve_model(wanted: &str, values: &[String]) -> Option<String> {
    if values.is_empty() || values.iter().any(|v| v == wanted) {
        return Some(wanted.to_owned());
    }
    values
        .iter()
        .find(|v| v.rsplit('/').next() == Some(wanted))
        .cloned()
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Map a canonical permission mode onto the agent's own mode ids (PLAN §2.7).
/// Falls back to the agent's normal mode — never to a more permissive one.
fn resolve_mode(mode: PermissionMode, values: &[String]) -> Option<String> {
    const NORMAL: &[&str] = &["default", "build", "agent", "code", "normal"];
    let preferred: &[&str] = match mode {
        PermissionMode::ReadOnly => &["plan", "readonly"],
        PermissionMode::AskAll => &["default", "manual", "ask"],
        PermissionMode::AcceptEdits => &["acceptedits", "autoedit"],
        PermissionMode::Auto => &["auto", "autoedit", "acceptedits"],
        PermissionMode::FullAccess => &["yolo", "bypasspermissions", "fullaccess"],
    };
    let find = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| values.iter().find(|v| norm(v) == *n))
            .cloned()
    };
    find(preferred).or_else(|| {
        (mode != PermissionMode::ReadOnly)
            .then(|| find(NORMAL))
            .flatten()
    })
}

/// Pick the agent's option id for a canonical decision.
fn pick_option(options: &[(String, String)], decision: DecisionKind) -> Option<String> {
    let order: &[&str] = match decision {
        DecisionKind::AllowOnce => &["allow_once", "allow_always"],
        DecisionKind::AllowSession => &["allow_always", "allow_once"],
        DecisionKind::Deny | DecisionKind::DenyAndStop => &["reject_once", "reject_always"],
    };
    order.iter().find_map(|k| {
        options
            .iter()
            .find(|(kind, _)| kind == k)
            .map(|(_, id)| id.clone())
    })
}

/// Text of a `ContentBlock` (text and resource links; media is skipped).
fn content_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => str_at(block, "text"),
        Some("resource_link") => {
            let uri = str_at(block, "uri");
            let name = block.get("name").and_then(Value::as_str).unwrap_or(&uri);
            format!("[{name}]({uri})")
        }
        Some("resource") => block
            .pointer("/resource/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn relative(path: &str, cwd: &str) -> String {
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    path.strip_prefix(&prefix).unwrap_or(path).to_owned()
}

/// Minimal unified diff of a whole-text replacement (no context lines).
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (del, add) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    // A zero-length range starts at the line before it.
    let start = |n: usize| if n == 0 { head } else { head + 1 };
    let mut out = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{},{} +{},{} @@\n",
        start(del.len()),
        del.len(),
        start(add.len()),
        add.len()
    );
    for l in del {
        out.push_str(&format!("-{l}\n"));
    }
    for l in add {
        out.push_str(&format!("+{l}\n"));
    }
    out
}

/// Unified diffs for the `diff` blocks of a tool call's content.
fn diff_text(content: Option<&Value>, cwd: &str) -> String {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("diff"))
                .map(|b| {
                    unified_diff(
                        &relative(&str_at(b, "path"), cwd),
                        b.get("oldText").and_then(Value::as_str).unwrap_or_default(),
                        &str_at(b, "newText"),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Output text of a tool call: its content blocks (text, then diffs), else a
/// textual `rawOutput`.
fn tool_output(tool: &ToolState, cwd: &str) -> String {
    let text: Vec<String> = tool
        .content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("content"))
        .map(|b| content_text(b.get("content").unwrap_or(&Value::Null)))
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = text.join("\n");
    let diff = diff_text(Some(&Value::Array(tool.content.clone())), cwd);
    if !diff.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&diff);
    }
    if out.is_empty() {
        out = match &tool.raw_output {
            Value::String(s) => s.clone(),
            v => ["output", "stdout", "content", "result"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned(),
        };
    }
    out
}

/// `mcp__<server>__<tool>` for MCP tools of servers we attached. Agents name
/// them differently: Claude-style `mcp__server__tool`, OpenCode
/// `server_tool`, Gemini `tool (server MCP Server)`.
fn mcp_tool_name(tool: &ToolState, servers: &[String]) -> Option<String> {
    let name = tool.name.as_deref().unwrap_or_default();
    for candidate in [name, tool.title.as_str()] {
        if let Some(rest) = candidate.strip_prefix("mcp__")
            && rest.contains("__")
        {
            return Some(candidate.to_owned());
        }
    }
    for server in servers {
        for candidate in [name, tool.title.as_str()] {
            if let Some(t) = candidate.strip_prefix(&format!("{server}_"))
                && !t.is_empty()
                && !t.contains(' ')
            {
                return Some(format!("mcp__{server}__{t}"));
            }
        }
        let suffix = format!(" ({server} MCP Server)");
        if let Some(t) = tool.title.strip_suffix(&suffix) {
            return Some(format!("mcp__{server}__{t}"));
        }
    }
    None
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Canonical tool-call body from the merged ACP state.
fn tool_body(tool: &ToolState, cwd: &str, mcp_servers: &[String]) -> ItemBody {
    let kind = tool.kind.as_deref().unwrap_or("other");
    let raw = &tool.raw_input;
    let mut input = match raw {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        other => {
            let mut m = Map::new();
            m.insert("input".into(), other.clone());
            m
        }
    };
    let raw_str = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| raw.get(*k).and_then(Value::as_str))
            .map(str::to_owned)
    };
    let location = tool
        .locations
        .first()
        .and_then(|l| l.get("path"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let Some(full) = mcp_tool_name(tool, mcp_servers) {
        return ItemBody::ToolCall {
            tool: full.clone(),
            tool_kind: ToolKind::Mcp,
            title: full,
            input: Value::Object(input),
            output: None,
            is_error: false,
        };
    }
    let name = tool.name.as_deref().unwrap_or_default();
    let (tool_name, tool_kind) = match kind {
        "execute" => ("Shell".to_owned(), ToolKind::Command),
        "edit" | "delete" | "move" => (
            if name.is_empty() {
                capitalize(kind)
            } else {
                capitalize(name)
            },
            ToolKind::FileChange,
        ),
        "read" => ("Read".to_owned(), ToolKind::FileRead),
        "search" => (
            if name.is_empty() {
                "Search".to_owned()
            } else {
                capitalize(name)
            },
            ToolKind::Search,
        ),
        "fetch" => ("WebFetch".to_owned(), ToolKind::WebFetch),
        _ if matches!(name, "todowrite" | "todo_write" | "TodoWrite") => {
            ("TodoWrite".to_owned(), ToolKind::Todo)
        }
        _ if matches!(name, "task" | "Task" | "agent") => ("Agent".to_owned(), ToolKind::Subagent),
        _ => (
            if name.is_empty() {
                capitalize(kind)
            } else {
                capitalize(name)
            },
            ToolKind::Other,
        ),
    };
    match tool_kind {
        ToolKind::Command => {
            let command = raw_str(&["command", "cmd"]).or_else(|| {
                raw.get("command").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            });
            if let Some(c) = command {
                input.insert("command".into(), json!(c));
            }
        }
        ToolKind::FileChange | ToolKind::FileRead => {
            let path = location
                .clone()
                .or_else(|| raw_str(&["file_path", "filePath", "path", "absolute_path"]));
            if let Some(p) = path {
                input.insert("file_path".into(), json!(p));
            }
        }
        _ => {}
    }
    // A bare title ("shell", "glob") says nothing: show the real argument.
    let generic = tool.title.is_empty()
        || tool.title.eq_ignore_ascii_case(name)
        || tool.title.eq_ignore_ascii_case(&tool_name)
        || tool.title.eq_ignore_ascii_case(kind);
    let title = if generic {
        match tool_kind {
            ToolKind::Command => input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ToolKind::FileChange | ToolKind::FileRead => input
                .get("file_path")
                .and_then(Value::as_str)
                .map(|p| relative(p, cwd)),
            _ => raw_str(&["pattern", "query", "url", "path", "description", "prompt"]).or_else(
                || {
                    // Code-mode tools (OpenCode `execute`): the first line.
                    raw_str(&["code"]).and_then(|code| {
                        code.lines()
                            .map(str::trim)
                            .find(|l| !l.is_empty())
                            .map(|l| l.chars().take(100).collect())
                    })
                },
            ),
        }
        .unwrap_or_else(|| {
            if tool.title.is_empty() {
                tool_name.clone()
            } else {
                tool.title.clone()
            }
        })
    } else {
        tool.title.clone()
    };
    ItemBody::ToolCall {
        tool: tool_name,
        tool_kind,
        title,
        input: Value::Object(input),
        output: None,
        is_error: false,
    }
}

/// The models a session offers: a `configOptions` model select (flat or
/// grouped options) or the legacy `models.availableModels` list.
fn offered_models(result: &Value) -> Vec<loft_protocol::ModelInfo> {
    let info = |id: &str, name: Option<&str>, current: Option<&str>| loft_protocol::ModelInfo {
        id: id.to_owned(),
        display_name: name.unwrap_or(id).to_owned(),
        description: None,
        is_default: current == Some(id),
        ..Default::default()
    };
    if let Some(select) = result
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter().find(|o| {
                o.get("category").and_then(Value::as_str) == Some("model")
                    || o.get("id").and_then(Value::as_str) == Some("model")
            })
        })
    {
        let current = select.get("currentValue").and_then(Value::as_str);
        let mut out = Vec::new();
        for opt in select
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // Grouped options carry their own `options` list.
            let inner = opt.get("options").and_then(Value::as_array);
            for o in inner
                .map(|v| v.iter().collect::<Vec<_>>())
                .unwrap_or_else(|| vec![opt])
            {
                if let Some(id) = o.get("value").and_then(Value::as_str) {
                    out.push(info(id, o.get("name").and_then(Value::as_str), current));
                }
            }
        }
        return out;
    }
    let Some(models) = result.get("models") else {
        return Vec::new();
    };
    let current = models.get("currentModelId").and_then(Value::as_str);
    models
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("modelId").and_then(Value::as_str)?;
            Some(info(id, m.get("name").and_then(Value::as_str), current))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/opencode-2.2.5-turn.jsonl");

    fn events(out: &[Out]) -> Vec<&HarnessEvent> {
        out.iter()
            .filter_map(|o| match o {
                Out::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn sends(out: &[Out]) -> Vec<&Value> {
        out.iter()
            .filter_map(|o| match o {
                Out::Send(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn agentloft_mcp() -> McpServer {
        McpServer {
            name: "agentloft".into(),
            command: "/Apps/Agentloft/agentloftd".into(),
            args: vec!["mcp".into(), "--thread".into(), "T".into()],
        }
    }

    fn translator(resume: Option<&str>) -> Translator {
        Translator::new(
            "OpenCode",
            "/tmp/probe".into(),
            None,
            None,
            resume.map(str::to_owned),
            vec![],
        )
    }

    /// A translator with an open session `S` (no model/mode selectors).
    fn ready() -> Translator {
        let mut t = translator(None);
        t.initialize();
        t.on_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        );
        t.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S"}}"#);
        t
    }

    fn update(u: Value) -> String {
        json!({ "jsonrpc": "2.0", "method": "session/update",
                "params": { "sessionId": "S", "update": u } })
        .to_string()
    }

    fn permission(id: u64, tool_call: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
                "params": { "sessionId": "S", "toolCall": tool_call, "options": [
                    { "optionId": "once", "kind": "allow_once", "name": "Allow once" },
                    { "optionId": "always", "kind": "allow_always", "name": "Always allow" },
                    { "optionId": "reject", "kind": "reject_once", "name": "Reject" },
                ] } })
        .to_string()
    }

    #[test]
    fn offered_models_from_config_options_and_legacy_state() {
        let line = FIXTURE.lines().nth(1).unwrap();
        let v: Value = serde_json::from_str(line).unwrap();
        let models = offered_models(&v["result"]);
        assert!(models.len() >= 4, "{models:?}");
        let default: Vec<_> = models.iter().filter(|m| m.is_default).collect();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].id, "opencode/iceman-free");
        let legacy = json!({"models": {"currentModelId": "b", "availableModels": [
            {"modelId": "a", "name": "Model A"}, {"modelId": "b", "name": "Model B"}]}});
        let models = offered_models(&legacy);
        assert_eq!(models[1].display_name, "Model B");
        assert!(models[1].is_default && !models[0].is_default);
    }

    #[test]
    fn images_go_as_content_blocks_only_when_supported() {
        let image = || vec![("image/png".to_owned(), "iVBOR".to_owned())];
        // No `promptCapabilities.image`: text only, plus a notice.
        let mut t = ready();
        let out = t.start_turn_with("look", image());
        assert!(matches!(events(&out)[1], HarnessEvent::Notice { .. }));
        assert_eq!(
            sends(&out)[0]["params"]["prompt"].as_array().unwrap().len(),
            1
        );
        // Advertised: an image block after the text.
        let mut t = translator(None);
        t.initialize();
        t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"image":true}}}}"#);
        t.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S"}}"#);
        let out = t.start_turn_with("look", image());
        let prompt = &sends(&out)[0]["params"]["prompt"];
        assert_eq!(
            prompt[1],
            json!({"type": "image", "mimeType": "image/png", "data": "iVBOR"})
        );
    }

    #[test]
    fn handshake_then_session_then_queued_prompt() {
        let mut t = Translator::new(
            "OpenCode",
            "/tmp/probe".into(),
            None,
            None,
            None,
            vec![agentloft_mcp()],
        );
        let init = t.initialize();
        assert_eq!(init["jsonrpc"], "2.0");
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            false
        );
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
        // The prompt arrives before the session exists → queued.
        let out = t.start_turn("hi");
        assert!(matches!(events(&out)[..], [HarnessEvent::TurnStarted]));
        assert!(sends(&out).is_empty());

        let mut lines = FIXTURE.lines();
        let out = t.on_line(lines.next().unwrap());
        let new = sends(&out)[0];
        assert_eq!(new["method"], "session/new");
        assert_eq!(new["params"]["cwd"], "/tmp/probe");
        assert_eq!(
            new["params"]["mcpServers"][0],
            json!({ "name": "agentloft", "command": "/Apps/Agentloft/agentloftd",
                    "args": ["mcp", "--thread", "T"], "env": [] })
        );
        let out = t.on_line(lines.next().unwrap());
        let HarnessEvent::SessionReady {
            native_session_id,
            model,
            ..
        } = events(&out)[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(native_session_id, "ses_e40b386d1ffeSL1Vzm3xwq9E25");
        assert_eq!(model.as_deref(), Some("opencode/iceman-free"));
        let prompt = sends(&out)[0];
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["id"], 3);
        assert_eq!(prompt["params"]["sessionId"], *native_session_id);
        assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");
    }

    #[test]
    fn recorded_turn_replays_with_permissions() {
        let mut t = translator(None);
        t.initialize();
        let mut out = t.start_turn("x");
        let mut approvals = Vec::new();
        for line in FIXTURE.lines() {
            let produced = t.on_line(line);
            for o in &produced {
                if let Out::Event(HarnessEvent::RequestOpened {
                    native_id,
                    kind,
                    title,
                    detail,
                    options,
                    related_item,
                }) = o
                {
                    approvals.push((*kind, title.clone(), detail.clone()));
                    assert_eq!(
                        options,
                        &vec![
                            DecisionKind::AllowOnce,
                            DecisionKind::AllowSession,
                            DecisionKind::Deny,
                            DecisionKind::DenyAndStop
                        ]
                    );
                    assert!(related_item.is_some());
                    let reply = t.respond(native_id, DecisionKind::AllowOnce);
                    let v = sends(&reply)[0];
                    assert_eq!(v["id"].to_string(), *native_id, "echoes the agent's id");
                    assert_eq!(
                        v["result"],
                        json!({ "outcome": { "outcome": "selected", "optionId": "once" } })
                    );
                    out.extend(reply);
                }
            }
            out.extend(produced);
        }
        assert_eq!(approvals.len(), 2);
        assert_eq!(approvals[0].0, RequestKind::ToolApproval);
        assert_eq!(approvals[0].1, "Shell: touch acp-live.txt");
        assert_eq!(approvals[0].2["input"]["command"], "touch acp-live.txt");
        assert_eq!(approvals[1].0, RequestKind::FileChangeApproval);
        assert_eq!(approvals[1].1, "Edit: a.txt");
        assert!(
            approvals[1].2["diff"]
                .as_str()
                .unwrap()
                .contains("-hello\n+world")
        );

        let evs = events(&out);
        // Pending calls are shown once their real input is known.
        let started: Vec<(&str, ToolKind, &str)> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemStarted {
                    body:
                        ItemBody::ToolCall {
                            tool,
                            tool_kind,
                            title,
                            ..
                        },
                    ..
                } => Some((tool.as_str(), *tool_kind, title.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            started,
            vec![
                ("Shell", ToolKind::Command, "touch acp-live.txt"),
                ("Read", ToolKind::FileRead, "a.txt"),
                ("Edit", ToolKind::FileChange, "a.txt"),
            ]
        );
        let completed: Vec<(&ItemStatus, &str)> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemCompleted {
                    status,
                    tool_result: Some(r),
                    ..
                } => Some((status, r.output.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 3);
        assert!(completed.iter().all(|(s, _)| **s == ItemStatus::Completed));
        assert!(completed.iter().any(|(_, o)| *o == "(no output)"));
        let edit = completed
            .iter()
            .find(|(_, o)| o.starts_with("Edited a.txt"))
            .unwrap();
        assert!(
            edit.1
                .contains("--- a/a.txt\n+++ b/a.txt\n@@ -1,1 +1,1 @@\n-hello\n+world")
        );
        let text = |channel: DeltaChannel| -> String {
            evs.iter()
                .filter_map(|e| match e {
                    HarnessEvent::ItemDelta {
                        channel: c, text, ..
                    } if *c == channel => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(text(DeltaChannel::Text), "done");
        assert!(!text(DeltaChannel::Reasoning).is_empty());
        // Every started item is completed before the turn ends.
        let started_ids: Vec<&String> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemStarted { native_id, .. } => Some(native_id),
                _ => None,
            })
            .collect();
        for id in started_ids {
            assert!(
                evs.iter().any(|e| matches!(e, HarnessEvent::ItemCompleted { native_id, .. } if native_id == id)),
                "{id} never completed"
            );
        }
        match evs.last().unwrap() {
            HarnessEvent::TurnCompleted {
                status,
                usage,
                error,
            } => {
                assert_eq!(*status, TurnStatus::Completed);
                assert!(error.is_none());
                let u = usage.as_ref().expect("usage");
                assert_eq!(
                    (u.input_tokens, u.output_tokens, u.cache_read_tokens),
                    (7813, 74, 13312)
                );
                assert_eq!(u.cost_usd, Some(0.0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn load_suppresses_history_replay() {
        let mut t = translator(Some("ses_old"));
        t.initialize();
        let out =
            t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"resume":{}}}}}"#);
        let load = sends(&out)[0];
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["params"]["sessionId"], "ses_old");
        assert_eq!(load["params"]["mcpServers"], json!([]));
        // History replay before the response is not re-emitted.
        for u in [
            json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"t0","title":"x","kind":"execute","status":"completed"}),
        ] {
            let line = json!({"jsonrpc":"2.0","method":"session/update",
                              "params":{"sessionId":"ses_old","update":u}})
            .to_string();
            assert!(t.on_line(&line).is_empty());
        }
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
        assert!(matches!(
            events(&out)[0],
            HarnessEvent::SessionReady { native_session_id, .. } if native_session_id == "ses_old"
        ));
        // Live updates flow again.
        let out = t.on_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_old",
                    "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new"}}}})
            .to_string(),
        );
        assert_eq!(events(&out).len(), 2);
    }

    #[test]
    fn resume_falls_back_to_resume_or_new() {
        // Only `sessionCapabilities.resume`.
        let mut t = translator(Some("ses_old"));
        t.initialize();
        let out = t.on_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"sessionCapabilities":{"resume":{}}}}}"#,
        );
        assert_eq!(sends(&out)[0]["method"], "session/resume");
        // A lost session: notice, then a fresh one.
        let out = t.on_line(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32002,"message":"Resource not found"}}"#,
        );
        assert!(matches!(
            events(&out)[0],
            HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                ..
            }
        ));
        assert_eq!(sends(&out)[0]["method"], "session/new");

        // Neither: a new session straight away.
        let mut t = translator(Some("ses_old"));
        t.initialize();
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
        assert!(matches!(events(&out)[0], HarnessEvent::Notice { .. }));
        assert_eq!(sends(&out)[0]["method"], "session/new");
    }

    #[test]
    fn cancel_answers_open_permissions() {
        let mut t = ready();
        let out = t.start_turn("go");
        assert_eq!(sends(&out)[0]["method"], "session/prompt");
        let out = t.on_line(&permission(
            0,
            json!({"toolCallId":"c1","name":"shell","title":"sleep 30","kind":"execute",
                   "status":"pending","rawInput":{"command":"sleep 30"}}),
        ));
        assert!(
            matches!(events(&out)[..], [HarnessEvent::ItemStarted { .. }, HarnessEvent::RequestOpened { native_id, .. }] if native_id == "0")
        );
        let out = t.interrupt();
        let s = sends(&out);
        assert_eq!(s[0]["method"], "session/cancel");
        assert!(s[0].get("id").is_none(), "a notification");
        assert_eq!(s[0]["params"]["sessionId"], "S");
        assert_eq!(s[1]["id"], 0);
        assert_eq!(s[1]["result"]["outcome"]["outcome"], "cancelled");
        assert!(
            matches!(events(&out)[0], HarnessEvent::RequestResolved { native_id } if native_id == "0")
        );
        t.on_line(&update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed"}),
        ));
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#);
        assert!(matches!(
            events(&out).last().unwrap(),
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn deny_and_stop_rejects_then_cancels() {
        let mut t = ready();
        t.start_turn("go");
        t.on_line(&permission(
            7,
            json!({"toolCallId":"c1","title":"rm -rf x","kind":"execute","rawInput":{"command":"rm -rf x"}}),
        ));
        let out = t.respond("7", DecisionKind::DenyAndStop);
        let s = sends(&out);
        assert_eq!(s[0]["id"], 7);
        assert_eq!(
            s[0]["result"]["outcome"],
            json!({"outcome":"selected","optionId":"reject"})
        );
        assert_eq!(s[1]["method"], "session/cancel");
        // A plain deny keeps the turn going.
        t.on_line(&permission(
            8,
            json!({"toolCallId":"c2","title":"ls","kind":"execute"}),
        ));
        let out = t.respond("8", DecisionKind::Deny);
        assert_eq!(sends(&out).len(), 1);
    }

    #[test]
    fn stop_reasons_map_to_turn_status() {
        for (reason, expect, has_error) in [
            ("end_turn", TurnStatus::Completed, false),
            ("cancelled", TurnStatus::Interrupted, false),
            ("refusal", TurnStatus::Failed, true),
            ("max_tokens", TurnStatus::Failed, true),
            ("max_turn_requests", TurnStatus::Failed, true),
            ("something_new", TurnStatus::Completed, false),
        ] {
            let mut t = ready();
            t.start_turn("go");
            let out = t.on_line(
                &json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":reason}}).to_string(),
            );
            let HarnessEvent::TurnCompleted { status, error, .. } = events(&out).last().unwrap()
            else {
                panic!()
            };
            assert_eq!((*status, error.is_some()), (expect, has_error), "{reason}");
        }
        // A JSON-RPC error fails the turn.
        let mut t = ready();
        t.start_turn("go");
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"boom"}}"#);
        assert!(matches!(
            events(&out).last().unwrap(),
            HarnessEvent::TurnCompleted { status: TurnStatus::Failed, error: Some(e), .. } if e == "OpenCode: boom"
        ));
    }

    #[test]
    fn mcp_tools_of_allowed_servers_are_auto_approved() {
        let mut t = Translator::new(
            "OpenCode",
            "/tmp/probe".into(),
            None,
            None,
            None,
            vec![agentloft_mcp()],
        );
        t.auto_allow_mcp = vec!["agentloft".into()];
        t.initialize();
        t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
        t.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S"}}"#);
        t.start_turn("go");
        // OpenCode `server_tool`, Claude-style `mcp__server__tool`, Gemini's
        // display title.
        for (i, call) in [
            json!({"toolCallId":"m1","name":"agentloft_list_threads","title":"agentloft_list_threads","kind":"other"}),
            json!({"toolCallId":"m2","title":"mcp__agentloft__read_thread","kind":"other"}),
            json!({"toolCallId":"m3","title":"browser_open (agentloft MCP Server)","kind":"other"}),
        ]
        .into_iter()
        .enumerate()
        {
            let out = t.on_line(&permission(i as u64, call));
            assert!(events(&out).is_empty(), "{out:?}");
            assert_eq!(
                sends(&out)[0]["result"]["outcome"],
                json!({"outcome":"selected","optionId":"once"})
            );
        }
        // Another server's tools still ask.
        let out = t.on_line(&permission(
            9,
            json!({"toolCallId":"g1","title":"mcp__github__create_issue","kind":"other","rawInput":{"title":"x"}}),
        ));
        let evs = events(&out);
        let HarnessEvent::RequestOpened { title, .. } = evs.last().unwrap() else {
            panic!("{out:?}")
        };
        assert_eq!(title, "MCP: github · create_issue");
        // Auto-approved calls still render as MCP tool items.
        let out = t.on_line(&update(json!({"sessionUpdate":"tool_call_update","toolCallId":"m1",
            "status":"completed","content":[{"type":"content","content":{"type":"text","text":"[]"}}]})));
        let HarnessEvent::ItemStarted {
            body: ItemBody::ToolCall {
                tool, tool_kind, ..
            },
            ..
        } = events(&out)[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(
            (tool.as_str(), *tool_kind),
            ("mcp__agentloft__list_threads", ToolKind::Mcp)
        );
    }

    #[test]
    fn model_and_mode_follow_config_options() {
        let mut t = Translator::new(
            "OpenCode",
            "/tmp/probe".into(),
            Some("claude-opus-5-5".into()),
            Some(PermissionMode::ReadOnly),
            None,
            vec![],
        );
        t.initialize();
        t.start_turn("go");
        let mut lines = FIXTURE.lines();
        t.on_line(lines.next().unwrap());
        let out = t.on_line(lines.next().unwrap());
        let s = sends(&out);
        assert_eq!(s.len(), 2, "prompt waits for the settings: {s:?}");
        assert_eq!(s[0]["method"], "session/set_config_option");
        assert_eq!(s[0]["params"]["configId"], "model");
        assert_eq!(s[0]["params"]["value"], "anthropic/claude-opus-5-5");
        assert_eq!(s[1]["params"]["configId"], "mode");
        assert_eq!(s[1]["params"]["value"], "plan");
        assert!(sends(&t.on_line(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#)).is_empty());
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
        assert_eq!(sends(&out)[0]["method"], "session/prompt");
        // OpenCode only has build/plan: full access falls back to build.
        let out = t.set_permission_mode(Some(PermissionMode::FullAccess));
        assert_eq!(sends(&out)[0]["params"]["value"], "build");
        // Harness default leaves the agent alone.
        assert!(t.set_permission_mode(None).is_empty());
        // An unknown model is reported, not sent.
        let out = t.set_model(Some("gpt-nope".into()));
        assert!(sends(&out).is_empty());
        assert!(matches!(events(&out)[0], HarnessEvent::Notice { .. }));
    }

    #[test]
    fn legacy_modes_and_missing_selectors() {
        let mut t = ready();
        assert!(matches!(
            events(&t.set_model(Some("m".into())))[0],
            HarnessEvent::Notice { .. }
        ));
        let mut t = translator(None);
        t.initialize();
        t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
        t.on_line(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S","modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"},{"id":"autoEdit","name":"Auto edit"},{"id":"yolo","name":"YOLO"}]},"models":{"currentModelId":"gemini-3-pro","availableModels":[{"modelId":"gemini-3-pro","name":"Pro"},{"modelId":"gemini-3-flash","name":"Flash"}]}}}"#,
        );
        let out = t.set_permission_mode(Some(PermissionMode::FullAccess));
        assert_eq!(
            sends(&out)[0]["params"],
            json!({"sessionId":"S","modeId":"yolo"})
        );
        assert_eq!(sends(&out)[0]["method"], "session/set_mode");
        let out = t.set_permission_mode(Some(PermissionMode::AcceptEdits));
        assert_eq!(sends(&out)[0]["params"]["modeId"], "autoEdit");
        // No read-only mode: never fall back to a more permissive one.
        let out = t.set_permission_mode(Some(PermissionMode::ReadOnly));
        assert!(sends(&out).is_empty());
        let out = t.set_model(Some("gemini-3-flash".into()));
        assert_eq!(sends(&out)[0]["method"], "session/set_model");
        assert_eq!(sends(&out)[0]["params"]["modelId"], "gemini-3-flash");
    }

    #[test]
    fn chunks_split_into_items() {
        let mut t = ready();
        t.start_turn("go");
        let mut out = Vec::new();
        for u in [
            json!({"sessionUpdate":"agent_thought_chunk","messageId":"m1:r","content":{"type":"text","text":"think"}}),
            json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"a"}}),
            json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"b"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"ls","kind":"execute","status":"in_progress","rawInput":{"command":"ls"}}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","content":[{"type":"content","content":{"type":"text","text":"a.txt\n"}}]}),
            json!({"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"type":"text","text":"c"}}),
        ] {
            out.extend(t.on_line(&update(u)));
        }
        out.extend(t.on_line(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#));
        let kinds: Vec<String> = events(&out)
            .iter()
            .map(|e| match e {
                HarnessEvent::ItemStarted { native_id, .. } => format!("start {native_id}"),
                HarnessEvent::ItemDelta {
                    native_id, text, ..
                } => format!("delta {native_id} {text}"),
                HarnessEvent::ItemCompleted { native_id, .. } => format!("done {native_id}"),
                HarnessEvent::TurnCompleted { .. } => "turn".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "start acp:thought:1",
                "delta acp:thought:1 think",
                "done acp:thought:1",
                "start acp:msg:2",
                "delta acp:msg:2 a",
                "delta acp:msg:2 b",
                "done acp:msg:2",
                "start t1",
                "delta t1 a.txt\n",
                "start acp:msg:3",
                "delta acp:msg:3 c",
                "done acp:msg:3",
                "done t1",
                "turn",
            ]
        );
    }

    #[test]
    fn plan_becomes_a_todo_item() {
        let mut t = ready();
        t.start_turn("go");
        let out = t.on_line(&update(json!({"sessionUpdate":"plan","entries":[
            {"content":"Edit a.txt","priority":"high","status":"completed"},
            {"content":"Reply","priority":"medium","status":"in_progress"}]})));
        let evs = events(&out);
        let HarnessEvent::ItemStarted {
            body:
                ItemBody::ToolCall {
                    tool_kind,
                    title,
                    input,
                    ..
                },
            ..
        } = evs[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(*tool_kind, ToolKind::Todo);
        assert_eq!(title, "1/2 done");
        assert_eq!(input["todos"][1]["status"], "in_progress");
        let HarnessEvent::ItemCompleted {
            tool_result: Some(r),
            ..
        } = evs[1]
        else {
            panic!()
        };
        assert_eq!(r.output, "[x] Edit a.txt\n[~] Reply");
    }

    #[test]
    fn unsupported_agent_requests_are_answered() {
        let mut t = ready();
        let out = t.on_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"fs/read_text_file","params":{"path":"/x"}}"#,
        );
        let v = sends(&out)[0];
        assert_eq!(v["id"], 5);
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn auth_failure_fails_turns() {
        let mut t = translator(None);
        t.initialize();
        t.start_turn("go");
        t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{},"authMethods":[{"id":"login","name":"Login","description":"Run `opencode auth login` in the terminal"}]}}"#);
        let out = t.on_line(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required"}}"#,
        );
        let HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            error: Some(e),
            ..
        } = events(&out)[0]
        else {
            panic!("{out:?}")
        };
        assert!(e.contains("opencode auth login"), "{e}");
        let out = t.start_turn("again");
        assert!(matches!(
            events(&out)[..],
            [
                HarnessEvent::TurnStarted,
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn interrupt_before_the_session_drops_the_prompt() {
        let mut t = translator(None);
        t.initialize();
        t.start_turn("go");
        let out = t.interrupt();
        assert!(matches!(
            events(&out)[..],
            [HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            }]
        ));
        t.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
        let out = t.on_line(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S"}}"#);
        assert!(sends(&out).is_empty(), "nothing left to prompt");
    }

    #[test]
    fn unified_diff_hunks() {
        assert_eq!(
            unified_diff("f", "a\nb\nc\n", "a\nB\nc\n"),
            "--- a/f\n+++ b/f\n@@ -2,1 +2,1 @@\n-b\n+B\n"
        );
        assert_eq!(
            unified_diff("f", "", "x\n"),
            "--- a/f\n+++ b/f\n@@ -0,0 +1,1 @@\n+x\n"
        );
    }
}
