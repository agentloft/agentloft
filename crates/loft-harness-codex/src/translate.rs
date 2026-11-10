//! Pure translation between `codex app-server` JSON-RPC (JSONL, no
//! `jsonrpc` field) and canonical harness events. No I/O, so recorded
//! sessions in `fixtures/` replay in tests.

use loft_harness::{HarnessEvent, ToolResult};
use loft_protocol::{
    DecisionKind, DeltaChannel, ItemBody, ItemStatus, NoticeLevel, PermissionMode, Question,
    QuestionOption, RequestKind, RequestReply, ToolKind, TurnStatus, Usage,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone)]
pub enum Out {
    Event(HarnessEvent),
    Send(Value),
    /// The answer to a query started with [`Translator::query`].
    Reply {
        query: u64,
        result: Result<Value, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    ThreadStart,
    ThreadResume,
    ThreadFork,
    TurnStart,
    /// The skills Codex has (`skills/list`).
    Skills,
    /// A request whose answer (or error) doesn't matter.
    Quiet,
    /// `/goal`: show the goal that comes back.
    GoalShow,
    /// `/goal pause|resume|clear`: say this once it's done.
    GoalSay(&'static str),
    /// A message for the running turn (`turn/steer`); if the turn has
    /// ended meanwhile, it starts the next one.
    Steer(String, Vec<String>),
    /// One part of a status query.
    Query {
        query: u64,
        part: QueryPart,
    },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPart {
    Account,
    RateLimits,
    Usage,
}

impl QueryPart {
    const ALL: [QueryPart; 3] = [QueryPart::Account, QueryPart::RateLimits, QueryPart::Usage];

    fn key(self) -> &'static str {
        match self {
            QueryPart::Account => "account",
            QueryPart::RateLimits => "rateLimits",
            QueryPart::Usage => "usage",
        }
    }

    fn method(self) -> &'static str {
        match self {
            QueryPart::Account => "account/read",
            QueryPart::RateLimits => "account/rateLimits/read",
            QueryPart::Usage => "account/usage/read",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalKind {
    /// `item/commandExecution|fileChange/requestApproval` → `{decision}`.
    Decision,
    /// MCP tool call via `mcpServer/elicitation/request` → `{action}`.
    McpTool,
    /// Questions via `item/tool/requestUserInput` → `{answers}`.
    UserInput,
}

#[derive(Debug, Clone)]
struct OpenApproval {
    kind: ApprovalKind,
    /// The server's JSON-RPC id, echoed back verbatim.
    rpc_id: Value,
    available: Vec<Value>,
    command: Option<Vec<String>>,
}

const AGENTLOFT_INSTRUCTIONS: &str = "You are running inside Agentloft, which shows each \
conversation as a thread in its sidebar. When the user asks you to spawn, start or create \
threads (or Agentloft agents), use the `agentloft` MCP tools: `spawn_thread` for each, then \
`wait_for_threads` for their replies. Do not use your built-in subagents for that; use them \
only when the user asks for subagents. Spawned threads use this thread's agent, model, \
reasoning effort and permissions unless the user names others. To start a thread in another \
project, pass its name as `project` to `spawn_thread` (never edit Agentloft itself to do it); \
for a chat with no project, pass `chat`.";

#[derive(Debug, Default)]
pub struct Translator {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Session settings applied at thread start / per turn.
    pub cwd: String,
    pub model: Option<String>,
    /// Reasoning effort, sent with every `turn/start`.
    pub effort: Option<String>,
    pub mode: Option<PermissionMode>,
    resume: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// Prompts queued until the thread exists.
    queued: VecDeque<(String, Vec<String>)>,
    approvals: HashMap<String, OpenApproval>,
    usage: Option<Usage>,
    interrupted: bool,
    /// MCP servers whose tool calls are approved automatically.
    pub auto_allow_mcp: Vec<String>,
    /// Open the session as a native fork of this Codex thread.
    pub fork_from: Option<String>,
    /// With `fork_from`: the last turn to keep (`lastTurnId`).
    pub fork_at: Option<String>,
    /// The server answered `initialize` (other requests may go out).
    ready: bool,
    /// Status queries: their parts so far, and those asked before `ready`.
    queries: HashMap<u64, serde_json::Map<String, Value>>,
    deferred: Vec<u64>,
    next_query: u64,
    /// The thread's latest `tokenUsage` (totals and the context window).
    token_usage: Option<Value>,
    /// Skills by name → their SKILL.md (sent with a `$name` mention).
    skills: Vec<(String, String)>,
    /// We asked for a turn and its `turn/started` hasn't come yet (any other
    /// `turn/started` is Codex starting one itself, e.g. to pursue a goal).
    own_turn_pending: bool,
    /// Messages for our turn that came before Codex told us its id; they
    /// go out as `turn/steer` once it does.
    steer_backlog: Vec<(String, Vec<String>)>,
    /// Set the goal active once our turn toward it has started.
    activate_goal: bool,
    next_note: u64,
    /// Codex's own subagents (collab `spawnAgent`): their thread → the
    /// spawn call's item, whose card shows them and holds their steps.
    agents: HashMap<String, String>,
    /// What each spawn card is called (by item id), for wait/message rows.
    spawn_titles: HashMap<String, String>,
    /// Spawn cards still running: item id → the agent's latest reply.
    open_spawns: HashMap<String, String>,
    /// Spawns whose agent thread hasn't been seen yet, oldest first.
    unclaimed: VecDeque<String>,
}

/// What `/init` asks Codex to do (Codex's own command isn't available over
/// the app-server).
const INIT_PROMPT: &str = "Create an AGENTS.md file at the root of this repository with instructions for coding agents working here: a short overview of the project and its layout, how to build, run and test it, the code style and conventions in use, and anything easy to get wrong. Base it on the code itself (read the build files, README and a sample of the source), keep it concise, and don't invent commands that don't exist.";

/// `(name, path, description)` of the enabled skills in a `skills/list`
/// result.
pub fn skills_from(result: &Value) -> Vec<(String, String, String)> {
    let mut list: Vec<(String, String, String)> = Vec::new();
    for entry in result
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for s in entry
            .get("skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if s.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let (Some(name), Some(path)) = (
                s.get("name").and_then(Value::as_str),
                s.get("path").and_then(Value::as_str),
            ) else {
                continue;
            };
            if list.iter().any(|(n, ..)| n == name) {
                continue;
            }
            let description = s
                .pointer("/interface/shortDescription")
                .or_else(|| s.get("shortDescription"))
                .or_else(|| s.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            list.push((name.to_owned(), path.to_owned(), description));
        }
    }
    list
}

/// The commands the composer offers for Codex: the ones Agentloft runs
/// (through the app-server or its own UI), then the skills.
pub fn codex_commands(skills: &[(String, String, String)]) -> Vec<loft_protocol::SlashCommandInfo> {
    let mut out = loft_protocol::codex_builtin_commands();
    out.extend(
        skills
            .iter()
            .map(|(name, _, description)| loft_protocol::SlashCommandInfo {
                name: name.clone(),
                description: description.clone(),
                argument_hint: String::new(),
                builtin: false,
                insert: format!("${name}"),
            }),
    );
    out
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

    /// A spawned subagent finished: its card completes with its reply.
    fn finish_spawn(&mut self, spawn: &str, failed: bool) -> Vec<Out> {
        let Some(reply) = self.open_spawns.remove(spawn) else {
            return vec![];
        };
        vec![Out::Event(HarnessEvent::ItemCompleted {
            native_id: spawn.to_owned(),
            body: None,
            tool_result: Some(ToolResult {
                output: reply,
                is_error: failed,
            }),
            status: if failed {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            },
        })]
    }

    /// Notifications from a subagent's thread: items become steps of its
    /// card (ids prefixed: each thread numbers its own items); its turn
    /// ending completes the card.
    fn on_agent_notification(&mut self, tid: String, method: &str, p: &Value) -> Vec<Out> {
        let spawn = match self.agents.get(&tid) {
            Some(s) => s.clone(),
            None => match self.unclaimed.pop_front() {
                Some(s) => {
                    self.agents.insert(tid.clone(), s.clone());
                    s
                }
                None => return vec![],
            },
        };
        let local = |id: &str| format!("{tid}:{id}");
        match method {
            "item/started" => {
                let item = p.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("collabAgentToolCall") {
                    return vec![];
                }
                match item_started(item, &self.cwd) {
                    Some(HarnessEvent::ItemStarted {
                        native_id, body, ..
                    }) => {
                        vec![Out::Event(HarnessEvent::ItemStarted {
                            native_id: local(&native_id),
                            body,
                            parent: Some(spawn),
                        })]
                    }
                    _ => vec![],
                }
            }
            "item/completed" => {
                let item = p.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                    && let Some(reply) = self.open_spawns.get_mut(&spawn)
                {
                    *reply = text.to_owned();
                }
                match item_completed(item, &self.cwd) {
                    Some(HarnessEvent::ItemCompleted {
                        native_id,
                        body,
                        tool_result,
                        status,
                    }) => vec![Out::Event(HarnessEvent::ItemCompleted {
                        native_id: local(&native_id),
                        body,
                        tool_result,
                        status,
                    })],
                    _ => vec![],
                }
            }
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta"
            | "item/commandExecution/outputDelta"
            | "item/fileChange/outputDelta" => {
                let channel = match method {
                    "item/agentMessage/delta" => DeltaChannel::Text,
                    "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                        DeltaChannel::Reasoning
                    }
                    _ => DeltaChannel::Output,
                };
                delta(p, channel)
                    .into_iter()
                    .map(|o| match o {
                        Out::Event(HarnessEvent::ItemDelta {
                            native_id,
                            channel,
                            text,
                        }) => Out::Event(HarnessEvent::ItemDelta {
                            native_id: local(&native_id),
                            channel,
                            text,
                        }),
                        other => other,
                    })
                    .collect()
            }
            "turn/completed" => {
                let failed = p.pointer("/turn/status").and_then(Value::as_str) == Some("failed");
                self.finish_spawn(&spawn, failed)
            }
            _ => vec![],
        }
    }

    /// Codex's subagent lifecycle (`subAgentActivity`): `started` is a new
    /// agent (its card, titled from its path: `/root/count_files` →
    /// "Count files"), `completed` / `interrupted` end it. Messages between
    /// agents (`interacted`) aren't shown.
    fn on_subagent_activity(&mut self, completed: bool, item: &Value) -> Vec<Out> {
        let get = |k: &str| item.get(k).and_then(Value::as_str);
        let (Some(id), Some(agent)) = (get("id"), get("agentThreadId")) else {
            return vec![];
        };
        match get("kind") {
            Some("started") if !completed => {
                let leaf = get("agentPath")
                    .and_then(|p| p.rsplit('/').next())
                    .filter(|l| !l.is_empty())
                    .unwrap_or("agent")
                    .replace(['_', '-'], " ");
                let mut chars = leaf.chars();
                let title = chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default();
                self.agents.insert(agent.to_owned(), id.to_owned());
                self.spawn_titles.insert(id.to_owned(), title.clone());
                self.open_spawns.insert(id.to_owned(), String::new());
                vec![Out::Event(HarnessEvent::ItemStarted {
                    native_id: id.to_owned(),
                    body: ItemBody::ToolCall {
                        tool: "Agent".into(),
                        tool_kind: ToolKind::Subagent,
                        title: title.clone(),
                        input: json!({
                            "description": title,
                            "model": item.get("model"),
                            "effort": item.get("reasoningEffort"),
                            "path": get("agentPath"),
                        }),
                        output: None,
                        is_error: false,
                    },
                    parent: None,
                })]
            }
            Some("completed" | "interrupted") if completed => match self.agents.get(agent).cloned()
            {
                Some(spawn) => self.finish_spawn(&spawn, false),
                None => vec![],
            },
            _ => vec![],
        }
    }

    /// Codex's collab tools: `spawnAgent` is an agent card (running until
    /// its agent's turn ends); the others are rows naming the agents.
    fn on_collab(&mut self, completed: bool, item: &Value) -> Vec<Out> {
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return vec![];
        };
        let tool = item.get("tool").and_then(Value::as_str).unwrap_or_default();
        let prompt = item
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let receivers: Vec<String> = item
            .get("receiverThreadIds")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let failed = item.get("status").and_then(Value::as_str) == Some("failed");
        let names = |me: &Self| {
            // No targets given: the agents still running.
            let targets: Vec<String> = if receivers.is_empty() {
                me.agents
                    .iter()
                    .filter(|(_, s)| me.open_spawns.contains_key(*s))
                    .map(|(t, _)| t.clone())
                    .collect()
            } else {
                receivers.clone()
            };
            let list: Vec<String> = targets
                .iter()
                .map(|r| {
                    me.agents
                        .get(r)
                        .and_then(|s| me.spawn_titles.get(s))
                        .map(|t| format!("“{t}”"))
                        .unwrap_or_else(|| "an agent".into())
                })
                .collect();
            let mut list = list;
            list.sort();
            list.join(", ")
        };
        // Agents Codex reports as finished.
        let finished: Vec<(String, bool)> = item
            .get("agentsStates")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(tid, st)| {
                        let status = st.get("status").and_then(Value::as_str)?;
                        let done = matches!(
                            status,
                            "completed" | "errored" | "shutdown" | "interrupted" | "notFound"
                        );
                        done.then(|| (tid.clone(), status == "errored"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        if tool == "spawnAgent" {
            if !completed {
                let title: String = prompt
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("Codex agent")
                    .chars()
                    .take(80)
                    .collect();
                self.spawn_titles.insert(id.clone(), title.clone());
                self.open_spawns.insert(id.clone(), String::new());
                self.unclaimed.push_back(id.clone());
                out.push(Out::Event(HarnessEvent::ItemStarted {
                    native_id: id,
                    body: ItemBody::ToolCall {
                        tool: "Agent".into(),
                        tool_kind: ToolKind::Subagent,
                        title: title.clone(),
                        input: json!({
                            "prompt": prompt,
                            "description": title,
                            "model": item.get("model"),
                            "effort": item.get("reasoningEffort"),
                        }),
                        output: None,
                        is_error: false,
                    },
                    parent: None,
                }));
                return out;
            }
            for r in &receivers {
                self.unclaimed.retain(|s| s != &id);
                self.agents.insert(r.clone(), id.clone());
            }
            if failed {
                out.extend(self.finish_spawn(&id, true));
            }
        } else {
            let (name, title) = match tool {
                "wait" => ("Wait for agents", names(self)),
                "sendInput" | "sendMessage" | "followupTask" => (
                    "Message agent",
                    format!("{}: {}", names(self), prompt.lines().next().unwrap_or("")),
                ),
                "closeAgent" => ("Close agent", names(self)),
                "interruptAgent" => ("Stop agent", names(self)),
                "resumeAgent" => ("Resume agent", names(self)),
                _ => ("List agents", String::new()),
            };
            if !completed {
                out.push(Out::Event(HarnessEvent::ItemStarted {
                    native_id: id,
                    body: ItemBody::ToolCall {
                        tool: name.into(),
                        tool_kind: ToolKind::Other,
                        title,
                        input: json!({ "agents": receivers }),
                        output: None,
                        is_error: false,
                    },
                    parent: None,
                }));
                return out;
            }
            out.push(Out::Event(HarnessEvent::ItemCompleted {
                native_id: id,
                body: None,
                tool_result: None,
                status: if failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                },
            }));
        }
        for (tid, errored) in finished {
            if let Some(spawn) = self.agents.get(&tid).cloned() {
                if let Some(msg) = item
                    .pointer(&format!("/agentsStates/{tid}/message"))
                    .and_then(Value::as_str)
                    && let Some(reply) = self.open_spawns.get_mut(&spawn)
                    && reply.is_empty()
                {
                    *reply = msg.to_owned();
                }
                out.extend(self.finish_spawn(&spawn, errored));
            }
        }
        out
    }

    fn request(&mut self, kind: Pending, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.pending.insert(self.next_id, kind);
        json!({ "id": self.next_id, "method": method, "params": params })
    }

    /// Ask for a status report (account, limits, usage history); answered
    /// with `Out::Reply` carrying the returned query id.
    pub fn query(&mut self) -> (u64, Vec<Out>) {
        self.next_query += 1;
        let query = self.next_query;
        self.queries.insert(query, serde_json::Map::new());
        if !self.ready {
            self.deferred.push(query);
            return (query, vec![]);
        }
        (query, self.query_requests(query))
    }

    fn query_requests(&mut self, query: u64) -> Vec<Out> {
        QueryPart::ALL
            .iter()
            .map(|&part| {
                Out::Send(self.request(Pending::Query { query, part }, part.method(), json!({})))
            })
            .collect()
    }

    /// One part of `query` came back; the reply goes out once all have.
    fn query_part(&mut self, query: u64, part: QueryPart, value: Value) -> Vec<Out> {
        let Some(parts) = self.queries.get_mut(&query) else {
            return vec![];
        };
        parts.insert(part.key().to_owned(), value);
        if parts.len() < QueryPart::ALL.len() {
            return vec![];
        }
        let mut parts = self.queries.remove(&query).unwrap_or_default();
        parts.insert(
            "tokenUsage".into(),
            self.token_usage.clone().unwrap_or(Value::Null),
        );
        parts.insert(
            "session".into(),
            json!({
                "model": self.model,
                "effort": self.effort,
                "cwd": self.cwd,
                "threadId": self.thread_id,
            }),
        );
        vec![Out::Reply {
            query,
            result: Ok(Value::Object(parts)),
        }]
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

    /// Standing instructions for a session with the Agentloft MCP server:
    /// "threads" are Agentloft threads (Codex otherwise reaches for its own
    /// subagents).
    fn developer_instructions(&self) -> Option<&'static str> {
        self.auto_allow_mcp
            .iter()
            .any(|s| s == "agentloft")
            .then_some(AGENTLOFT_INSTRUCTIONS)
    }

    fn open_thread(&mut self) -> Value {
        let instructions = self.developer_instructions();
        match (self.resume.clone(), self.fork_from.clone()) {
            (Some(id), _) => {
                let mut params = json!({ "threadId": id });
                if let Some(text) = instructions {
                    params["developerInstructions"] = json!(text);
                }
                self.request(Pending::ThreadResume, "thread/resume", params)
            }
            (None, Some(parent)) => {
                // Native fork (ADR-0011), into this thread's own cwd.
                let mut params = json!({ "threadId": parent, "cwd": self.cwd });
                if let Some(at) = &self.fork_at {
                    params["lastTurnId"] = json!(at);
                }
                if let Some(text) = instructions {
                    params["developerInstructions"] = json!(text);
                }
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
                if let Some(text) = instructions {
                    params["developerInstructions"] = json!(text);
                }
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

    fn turn_start(&mut self, text: &str, images: &[String], thread: &str) -> Vec<Out> {
        let trimmed = text.trim();
        if let Some(rest) = trimmed.strip_prefix("/goal")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return self.goal_command(rest.trim(), thread);
        }
        vec![Out::Send(self.turn_start_request(text, images, thread))]
    }

    /// `/goal [objective | pause | resume | clear]`.
    fn goal_command(&mut self, args: &str, thread: &str) -> Vec<Out> {
        let id = json!({ "threadId": thread });
        let set = |status: &str| json!({ "threadId": thread, "status": status, "origin": "user" });
        match args {
            "" => vec![Out::Send(self.request(
                Pending::GoalShow,
                "thread/goal/get",
                id,
            ))],
            "pause" => vec![Out::Send(self.request(
                Pending::GoalSay("Goal paused."),
                "thread/goal/set",
                set("paused"),
            ))],
            "resume" => vec![Out::Send(self.request(
                Pending::GoalSay("Goal resumed: Codex picks it back up."),
                "thread/goal/set",
                set("active"),
            ))],
            "clear" => vec![Out::Send(self.request(
                Pending::GoalSay("Goal cleared."),
                "thread/goal/clear",
                json!({ "threadId": thread, "origin": "user" }),
            ))],
            objective => {
                // Paused until our turn starts, so Codex can't start one first.
                let clear = self.request(Pending::Quiet, "thread/goal/clear", id);
                let goal = self.request(
                    Pending::Quiet,
                    "thread/goal/set",
                    json!({ "threadId": thread, "objective": objective, "status": "paused", "origin": "user" }),
                );
                let prompt = format!(
                    "This thread's goal is already set. Do not call create_goal. Start working on it now: {objective}"
                );
                self.activate_goal = true;
                let turn = self.turn_start_request(&prompt, &[], thread);
                vec![Out::Send(clear), Out::Send(goal), Out::Send(turn)]
            }
        }
    }

    /// A reply from Agentloft itself that ends the current turn.
    fn say(&mut self, text: String) -> Vec<Out> {
        self.own_turn_pending = false;
        self.next_note += 1;
        let native_id = format!("loft-note-{}", self.next_note);
        vec![
            Out::Event(HarnessEvent::ItemStarted {
                parent: None,
                native_id: native_id.clone(),
                body: ItemBody::AssistantMessage {
                    text: String::new(),
                },
            }),
            Out::Event(HarnessEvent::ItemCompleted {
                native_id,
                body: Some(ItemBody::AssistantMessage { text }),
                tool_result: None,
                status: ItemStatus::Completed,
            }),
            Out::Event(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: None,
                error: None,
            }),
        ]
    }

    fn turn_start_request(&mut self, text: &str, images: &[String], thread: &str) -> Value {
        // Codex commands, run through the app-server as turns of their own.
        let trimmed = text.trim();
        if trimmed == "/compact" {
            return self.request(
                Pending::TurnStart,
                "thread/compact/start",
                json!({ "threadId": thread }),
            );
        }
        if let Some(rest) = trimmed.strip_prefix("/review")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let rest = rest.trim();
            let target = if rest.is_empty() {
                json!({ "type": "uncommittedChanges" })
            } else {
                json!({ "type": "custom", "instructions": rest })
            };
            return self.request(
                Pending::TurnStart,
                "review/start",
                json!({ "threadId": thread, "target": target }),
            );
        }
        let text = if trimmed == "/init" {
            INIT_PROMPT
        } else {
            text
        };
        let mut input = vec![json!({ "type": "text", "text": text })];
        // `$skill` mentions bring the skill along.
        for word in text.split_whitespace() {
            let name = word
                .trim_start_matches('$')
                .trim_end_matches(|c: char| !c.is_alphanumeric());
            if word.starts_with('$')
                && let Some((_, path)) = self.skills.iter().find(|(n, _)| n == name)
            {
                input.push(json!({ "type": "skill", "name": name, "path": path }));
            }
        }
        input.extend(
            images
                .iter()
                .map(|path| json!({ "type": "localImage", "path": path })),
        );
        let mut params = json!({
            "threadId": thread,
            "input": input,
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &self.effort {
            params["effort"] = json!(effort);
        }
        if let Some(mode) = self.mode {
            let (approval, sandbox) = turn_policy(mode);
            params["approvalPolicy"] = approval;
            params["sandboxPolicy"] = sandbox;
        }
        self.request(Pending::TurnStart, "turn/start", params)
    }

    pub fn start_turn(&mut self, text: &str) -> Vec<Out> {
        self.start_turn_with(text, &[])
    }

    /// A prompt with image file paths (`localImage` inputs).
    pub fn start_turn_with(&mut self, text: &str, images: &[String]) -> Vec<Out> {
        self.interrupted = false;
        self.usage = None;
        self.own_turn_pending = true;
        let mut out = vec![Out::Event(HarnessEvent::TurnStarted)];
        match self.thread_id.clone() {
            Some(thread) => out.extend(self.turn_start(text, images, &thread)),
            None => self.queued.push_back((text.to_owned(), images.to_vec())),
        }
        out
    }

    /// A message for the running turn (`turn/steer`); with no turn
    /// running, a new one.
    pub fn steer_with(&mut self, text: &str, images: &[String]) -> Vec<Out> {
        let (Some(thread), Some(turn)) = (self.thread_id.clone(), self.turn_id.clone()) else {
            if self.own_turn_pending || !self.queued.is_empty() {
                // Our turn is on its way: steer it once it has an id.
                self.steer_backlog.push((text.to_owned(), images.to_vec()));
                return vec![];
            }
            return self.start_turn_with(text, images);
        };
        let mut input = vec![json!({ "type": "text", "text": text })];
        input.extend(
            images
                .iter()
                .map(|path| json!({ "type": "localImage", "path": path })),
        );
        vec![Out::Send(self.request(
            Pending::Steer(text.to_owned(), images.to_vec()),
            "turn/steer",
            json!({ "threadId": thread, "expectedTurnId": turn, "input": input }),
        ))]
    }

    /// Send the messages that waited for the turn's id.
    fn flush_steers(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.steer_backlog)
            .into_iter()
            .flat_map(|(text, images)| self.steer_with(&text, &images))
            .collect()
    }

    /// Messages still waiting when the turn ended run as the next turn.
    fn leftover_steers(&mut self) -> Vec<Out> {
        let backlog = std::mem::take(&mut self.steer_backlog);
        if backlog.is_empty() {
            return vec![];
        }
        let (text, images) = backlog.into_iter().fold(
            (String::new(), Vec::new()),
            |(mut text, mut images), (t, i)| {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&t);
                images.extend(i);
                (text, images)
            },
        );
        self.start_turn_with(&text, &images)
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

    pub fn respond(
        &mut self,
        request_id: &str,
        decision: DecisionKind,
        reply: Option<RequestReply>,
    ) -> Vec<Out> {
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
            ApprovalKind::UserInput => {
                // Skipped (or stopped): no answers.
                let answers: serde_json::Map<String, Value> = reply
                    .filter(|_| {
                        matches!(
                            decision,
                            DecisionKind::AllowOnce | DecisionKind::AllowSession
                        )
                    })
                    .map(|r| r.answers)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| (a.id, json!({ "answers": a.answers })))
                    .collect();
                json!({ "answers": answers })
            }
        };
        let mut out = vec![
            Out::Send(json!({ "id": open.rpc_id, "result": result })),
            Out::Event(HarnessEvent::RequestResolved {
                native_id: request_id.to_owned(),
            }),
        ];
        // A question has no "cancel" answer: stop the turn ourselves.
        if open.kind == ApprovalKind::UserInput && decision == DecisionKind::DenyAndStop {
            out.extend(self.interrupt());
        }
        out
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
        match kind {
            Pending::Quiet => return vec![],
            Pending::GoalSay(text) => {
                return match v.get("error") {
                    Some(err) => self.say(format!(
                        "Codex: {}",
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("goal request failed")
                    )),
                    None => self.say(text.to_owned()),
                };
            }
            Pending::GoalShow => {
                let goal = v.pointer("/result/goal").filter(|g| !g.is_null());
                let text = match goal {
                    None => "No goal is set. Start one with `/goal <objective>`.".to_owned(),
                    Some(g) => format!(
                        "**Goal:** {}\n\nStatus: {}",
                        g.get("objective")
                            .and_then(Value::as_str)
                            .unwrap_or("(none)"),
                        g.get("status").and_then(Value::as_str).unwrap_or("unknown")
                    ),
                };
                return self.say(text);
            }
            _ => {}
        }
        if let Pending::Query { query, part } = kind {
            // A part that failed is just missing from the report.
            let value = v.get("result").cloned().unwrap_or(Value::Null);
            return self.query_part(query, part, value);
        }
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
                    self.own_turn_pending = false;
                    self.queued.clear();
                    let mut out = vec![Out::Event(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        usage: None,
                        error: Some(format!("Codex: {msg}")),
                    })];
                    // Messages for the failed turn aren't dropped: they
                    // come back as notices the user can resend.
                    for (text, _) in std::mem::take(&mut self.steer_backlog) {
                        out.push(Out::Event(HarnessEvent::Notice {
                            level: NoticeLevel::Warning,
                            text: format!("Not sent: {text}"),
                        }));
                    }
                    out
                }
                // The turn ended before the message got in: it starts the
                // next one.
                Pending::Steer(text, images) => self.start_turn_with(&text, &images),
                Pending::Skills => vec![Out::Event(HarnessEvent::CommandsAvailable(
                    codex_commands(&[]),
                ))],
                Pending::Quiet | Pending::GoalShow | Pending::GoalSay(_) => vec![],
                Pending::Other | Pending::Query { .. } => vec![Out::Event(HarnessEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("Codex: {msg}"),
                })],
            };
        }
        let result = v.get("result").unwrap_or(&Value::Null);
        match kind {
            Pending::Initialize => {
                self.ready = true;
                let mut out = vec![Out::Send(json!({ "method": "initialized" }))];
                out.push(Out::Send(self.open_thread()));
                for query in std::mem::take(&mut self.deferred) {
                    out.extend(self.query_requests(query));
                }
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
                    models: vec![],
                    native_session_id: thread.clone(),
                    model: result
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    slash_commands: vec![],
                })];
                let cwd = self.cwd.clone();
                out.push(Out::Send(self.request(
                    Pending::Skills,
                    "skills/list",
                    json!({ "cwds": [cwd] }),
                )));
                while let Some((text, images)) = self.queued.pop_front() {
                    out.extend(self.turn_start(&text, &images, &thread));
                }
                out
            }
            Pending::Skills => {
                let list = skills_from(result);
                self.skills = list
                    .iter()
                    .map(|(n, p, _)| (n.clone(), p.clone()))
                    .collect();
                vec![Out::Event(HarnessEvent::CommandsAvailable(codex_commands(
                    &list,
                )))]
            }
            Pending::TurnStart => {
                if let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_owned());
                }
                let steers = self.flush_steers();
                if !steers.is_empty() {
                    let mut out = steers;
                    if std::mem::take(&mut self.activate_goal)
                        && let Some(thread) = self.thread_id.clone()
                    {
                        out.push(Out::Send(self.request(
                            Pending::Quiet,
                            "thread/goal/set",
                            json!({ "threadId": thread, "status": "active", "origin": "user" }),
                        )));
                    }
                    return out;
                }
                // Our turn toward a new goal is running: let the goal go.
                if std::mem::take(&mut self.activate_goal)
                    && let Some(thread) = self.thread_id.clone()
                {
                    return vec![Out::Send(self.request(
                        Pending::Quiet,
                        "thread/goal/set",
                        json!({ "threadId": thread, "status": "active", "origin": "user" }),
                    ))];
                }
                vec![]
            }
            Pending::Other
            | Pending::Steer(..)
            | Pending::Query { .. }
            | Pending::Quiet
            | Pending::GoalShow
            | Pending::GoalSay(_) => vec![],
        }
    }

    fn on_notification(&mut self, method: &str, p: &Value) -> Vec<Out> {
        // A subagent's own thread: its steps go on its card, nothing else.
        if let (Some(tid), Some(main)) = (
            p.get("threadId").and_then(Value::as_str),
            self.thread_id.as_deref(),
        ) && tid != main
        {
            return self.on_agent_notification(tid.to_owned(), method, p);
        }
        if matches!(method, "item/started" | "item/completed") {
            match p.pointer("/item/type").and_then(Value::as_str) {
                Some("collabAgentToolCall") => {
                    let item = p.get("item").cloned().unwrap_or(Value::Null);
                    return self.on_collab(method == "item/completed", &item);
                }
                Some("subAgentActivity") => {
                    let item = p.get("item").cloned().unwrap_or(Value::Null);
                    return self.on_subagent_activity(method == "item/completed", &item);
                }
                _ => {}
            }
        }
        match method {
            "turn/started" => {
                if let Some(turn) = p.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn.to_owned());
                }
                if std::mem::take(&mut self.own_turn_pending) {
                    self.flush_steers()
                } else {
                    // Codex started this one itself (e.g. working toward a goal).
                    self.interrupted = false;
                    self.usage = None;
                    vec![Out::Event(HarnessEvent::TurnStarted)]
                }
            }
            "item/started" => item_started(p.get("item").unwrap_or(&Value::Null), &self.cwd)
                .map(|e| vec![Out::Event(e)])
                .unwrap_or_default(),
            "item/completed"
                if p.pointer("/item/type").and_then(Value::as_str) == Some("contextCompaction") =>
            {
                vec![Out::Event(HarnessEvent::Notice {
                    level: NoticeLevel::Info,
                    text: "Conversation compacted: earlier turns are summarized for Codex.".into(),
                })]
            }
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
                self.token_usage = p.get("tokenUsage").cloned();
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
                let marker = turn.get("id").and_then(Value::as_str).map(|id| {
                    Out::Event(HarnessEvent::TurnMarker {
                        native_turn: id.to_owned(),
                    })
                });
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
                // Subagents not heard back from end with the turn.
                let open: Vec<String> = self.open_spawns.keys().cloned().collect();
                for spawn in open {
                    out.extend(self.finish_spawn(&spawn, false));
                }
                self.turn_id = None;
                self.interrupted = false;
                out.extend(marker);
                out.push(Out::Event(HarnessEvent::TurnCompleted {
                    status,
                    usage,
                    error,
                }));
                out.extend(self.leftover_steers());
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
            "item/tool/requestUserInput" => {
                let questions = user_input_questions(p);
                let first = questions
                    .first()
                    .map(|q| q.question.clone())
                    .unwrap_or_default();
                self.approvals.insert(
                    native_id.clone(),
                    OpenApproval {
                        kind: ApprovalKind::UserInput,
                        rpc_id: id,
                        available: vec![],
                        command: None,
                    },
                );
                return vec![Out::Event(HarnessEvent::RequestOpened {
                    native_id,
                    kind: RequestKind::Question,
                    title: format!("Question: {first}"),
                    detail: json!({ "questions": questions }),
                    options: vec![
                        DecisionKind::AllowOnce,
                        DecisionKind::Deny,
                        DecisionKind::DenyAndStop,
                    ],
                    related_item: item_id,
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

/// `item/tool/requestUserInput`'s questions in canonical form.
fn user_input_questions(p: &Value) -> Vec<Question> {
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    p.get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|q| {
            let options: Vec<QuestionOption> = q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|o| QuestionOption {
                    label: text(o, "label"),
                    description: text(o, "description"),
                })
                .collect();
            let flag = |k: &str| q.get(k).and_then(Value::as_bool).unwrap_or(false);
            Question {
                id: text(q, "id"),
                header: text(q, "header"),
                question: text(q, "question"),
                multi_select: false,
                // No options: the answer can only be typed.
                allow_other: flag("isOther") || options.is_empty(),
                secret: flag("isSecret"),
                options,
            }
        })
        .collect()
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

/// `model/list` result → picker entries (hidden models left out).
pub fn models(result: &Value) -> Vec<loft_protocol::ModelInfo> {
    result["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| !m["hidden"].as_bool().unwrap_or(false))
        .filter_map(|m| {
            let id = m["id"].as_str().or_else(|| m["model"].as_str())?;
            Some(loft_protocol::ModelInfo {
                id: id.to_owned(),
                display_name: m["displayName"].as_str().unwrap_or(id).to_owned(),
                description: m["description"].as_str().map(str::to_owned),
                is_default: m["isDefault"].as_bool().unwrap_or(false),
                efforts: m["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["reasoningEffort"].as_str().map(str::to_owned))
                    .collect(),
                default_effort: m["defaultReasoningEffort"].as_str().map(str::to_owned),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_subagents_replay_as_cards() {
        // codex-cli 0.167.0: two subagents (`subAgentActivity`), each in its
        // own thread, then a summary on the main thread.
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        t.thread_id = Some("01a1c31c-a89e-7623-93a9-60eed70424c7".into());
        let mut out = t.start_turn("x");
        for line in include_str!("../fixtures/0.167.0-subagents.jsonl").lines() {
            out.extend(t.on_line(line));
        }
        let evs = events(&out);
        let cards: Vec<(&String, &String)> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ItemStarted {
                    native_id,
                    body:
                        ItemBody::ToolCall {
                            tool_kind: ToolKind::Subagent,
                            title,
                            ..
                        },
                    ..
                } => Some((native_id, title)),
                _ => None,
            })
            .collect();
        let titles: Vec<&str> = cards.iter().map(|c| c.1.as_str()).collect();
        assert_eq!(titles, ["Count files", "List functions"]);
        for (card, _) in &cards {
            // Each card got its agent's steps and was completed.
            assert!(evs.iter().any(
                |e| matches!(e, HarnessEvent::ItemStarted { parent: Some(p), .. } if p == *card)
            ));
            assert!(evs.iter().any(
                |e| matches!(e, HarnessEvent::ItemCompleted { native_id, .. } if native_id == *card)
            ));
        }
        // Nothing of theirs reached the main timeline, and only the main
        // turn completed.
        let top_messages = evs
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    HarnessEvent::ItemStarted {
                        body: ItemBody::AssistantMessage { .. },
                        parent: None,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(top_messages, 2);
        let turns = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::TurnCompleted { .. }))
            .count();
        assert_eq!(turns, 1);
    }

    #[test]
    fn codex_subagents_get_their_own_card_with_their_steps() {
        let mut t = Translator::new("/w".into(), None, None, None);
        t.thread_id = Some("T".into());
        let note = |method: &str, params: serde_json::Value| {
            serde_json::json!({ "method": method, "params": params }).to_string()
        };
        let collab = |status: &str,
                      tool: &str,
                      receivers: serde_json::Value,
                      states: serde_json::Value| {
            serde_json::json!({ "type": "collabAgentToolCall", "id": format!("c-{tool}"), "tool": tool,
                "status": status, "senderThreadId": "T", "receiverThreadIds": receivers,
                "agentsStates": states, "prompt": "Map the crates\nthen report" })
        };
        // The spawn call: a card titled from its prompt, still running when
        // Codex reports the call done.
        let out = t.on_line(&note("item/started", serde_json::json!({ "threadId": "T", "item": collab("inProgress", "spawnAgent", serde_json::json!([]), serde_json::json!({})) })));
        assert!(
            matches!(events(&out).as_slice(), [HarnessEvent::ItemStarted { native_id, body: ItemBody::ToolCall { tool_kind: ToolKind::Subagent, title, .. }, parent: None }]
            if native_id == "c-spawnAgent" && title == "Map the crates")
        );
        let out = t.on_line(&note("item/completed", serde_json::json!({ "threadId": "T", "item": collab("completed", "spawnAgent", serde_json::json!(["S"]), serde_json::json!({})) })));
        assert!(events(&out).is_empty(), "{out:?}");
        // The agent's own steps belong to the card, not the main timeline.
        let out = t.on_line(&note("item/started", serde_json::json!({ "threadId": "S", "item": { "type": "commandExecution", "id": "i1", "command": "ls" } })));
        assert!(
            matches!(events(&out).as_slice(), [HarnessEvent::ItemStarted { native_id, parent: Some(p), .. }]
            if native_id == "S:i1" && p == "c-spawnAgent")
        );
        let out = t.on_line(&note(
            "turn/started",
            serde_json::json!({ "threadId": "S", "turn": { "id": "x" } }),
        ));
        assert!(out.is_empty());
        // Waiting names the agent.
        let out = t.on_line(&note("item/started", serde_json::json!({ "threadId": "T", "item": collab("inProgress", "wait", serde_json::json!(["S"]), serde_json::json!({})) })));
        assert!(
            matches!(events(&out).as_slice(), [HarnessEvent::ItemStarted { body: ItemBody::ToolCall { tool, title, .. }, .. }]
            if tool == "Wait for agents" && title == "“Map the crates”")
        );
        t.on_line(&note("item/completed", serde_json::json!({ "threadId": "S", "item": { "type": "agentMessage", "id": "i2", "text": "Five crates." } })));
        // Its turn ending completes the card with its reply.
        let out = t.on_line(&note(
            "turn/completed",
            serde_json::json!({ "threadId": "S", "turn": { "id": "x", "status": "completed" } }),
        ));
        assert!(
            matches!(events(&out).as_slice(), [HarnessEvent::ItemCompleted { native_id, tool_result: Some(r), status: ItemStatus::Completed, .. }]
            if native_id == "c-spawnAgent" && r.output == "Five crates.")
        );
        // A thread nobody spawned is ignored.
        assert!(t.on_line(&note("item/started", serde_json::json!({ "threadId": "Z", "item": { "type": "agentMessage", "id": "q" } }))).is_empty());
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
    fn model_list_skips_hidden() {
        let result = serde_json::json!({"data": [
            {"id": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol", "isDefault": true, "hidden": false,
             "description": "Latest workhorse model for coding and everyday work."},
            {"id": "internal", "displayName": "Internal", "hidden": true},
        ]});
        let models = models(&result);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "GPT-6.1-Sol");
        assert!(models[0].is_default);
    }

    #[test]
    fn fork_through_an_earlier_turn() {
        let mut t = Translator::new("/tmp/wt".into(), None, None, None);
        t.fork_from = Some("parent-thread".into());
        t.fork_at = Some("turn-1".into());
        t.initialize();
        let out = t.on_line(r#"{"id":1,"result":{}}"#);
        let Out::Send(fork) = &out[1] else { panic!() };
        assert_eq!(fork["method"], "thread/fork");
        assert_eq!(fork["params"]["lastTurnId"], "turn-1");
    }

    #[test]
    fn turn_completion_reports_the_turn_id() {
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        t.thread_id = Some("T".into());
        t.start_turn("x");
        let out = t.on_line(
            r#"{"method":"turn/completed","params":{"turn":{"id":"turn-9","status":"completed"}}}"#,
        );
        assert!(events(&out).iter().any(
            |e| matches!(e, HarnessEvent::TurnMarker { native_turn } if native_turn == "turn-9")
        ));
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
    fn effort_rides_on_turn_start() {
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        t.thread_id = Some("T1".into());
        let Out::Send(turn) = t.start_turn("x").pop().unwrap() else {
            panic!()
        };
        assert!(turn["params"].get("effort").is_none(), "model default");
        t.effort = Some("high".into());
        let Out::Send(turn) = t.start_turn("y").pop().unwrap() else {
            panic!()
        };
        assert_eq!(turn["params"]["effort"], "high");
    }

    #[test]
    fn model_list_carries_efforts() {
        let models = models(&json!({"data": [{
            "id": "gpt-6", "displayName": "GPT-6", "isDefault": true, "hidden": false,
            "defaultReasoningEffort": "medium",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "low", "description": "Fast"},
                {"reasoningEffort": "medium", "description": "Balanced"}
            ]
        }]}));
        assert_eq!(models[0].efforts, ["low", "medium"]);
        assert_eq!(models[0].default_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn agentloft_sessions_learn_what_threads_are() {
        let mut t = Translator::new("/tmp/probe".into(), None, None, None);
        t.auto_allow_mcp = vec!["agentloft".into()];
        t.initialize();
        t.start_turn("hi");
        let out = t.on_line(r#"{"id":1,"result":{}}"#);
        let Out::Send(start) = &out[1] else { panic!() };
        assert_eq!(start["method"], "thread/start");
        let text = start["params"]["developerInstructions"].as_str().unwrap();
        assert!(text.contains("spawn_thread"), "{text}");
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
        assert!(start["params"].get("developerInstructions").is_none());
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
                    let reply = t.respond(native_id, DecisionKind::AllowOnce, None);
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
        let reply = t.respond(native_id, DecisionKind::Deny, None);
        let Out::Send(v) = &reply[0] else { panic!() };
        assert_eq!(v["id"], 4);
        assert_eq!(v["result"]["action"], "decline");
    }

    #[test]
    fn request_user_input_is_a_question() {
        let mut t = Translator::new("/w".into(), None, None, None);
        let out = t.on_line(
            r#"{"id":7,"method":"item/tool/requestUserInput","params":{"threadId":"t","turnId":"u","itemId":"i1","isBlocking":true,"questions":[{"id":"db","header":"Database","question":"Which DB?","isOther":true,"options":[{"label":"SQLite","description":"a file"}]},{"id":"key","header":"Key","question":"API key?","isSecret":true,"options":null}]}}"#,
        );
        let Out::Event(HarnessEvent::RequestOpened {
            native_id,
            kind,
            detail,
            ..
        }) = &out[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(*kind, RequestKind::Question);
        let qs: Vec<Question> = serde_json::from_value(detail["questions"].clone()).unwrap();
        assert_eq!((qs[0].id.as_str(), qs[0].allow_other), ("db", true));
        assert!(qs[1].secret && qs[1].allow_other && qs[1].options.is_empty());
        let reply = RequestReply {
            answers: vec![loft_protocol::QuestionAnswer {
                id: "db".into(),
                answers: vec!["SQLite".into()],
            }],
            message: None,
        };
        let out = t.respond(native_id, DecisionKind::AllowOnce, Some(reply));
        let Out::Send(v) = &out[0] else { panic!() };
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["answers"]["db"]["answers"][0], "SQLite");
    }

    #[test]
    fn status_query_collects_its_parts() {
        let mut t = Translator::new("/w".into(), None, None, None);
        // Before `initialize` answers, the query waits.
        let (q, outs) = t.query();
        assert!(outs.is_empty());
        let init = t.initialize();
        let init_id = init["id"].as_u64().unwrap();
        let outs = t.on_line(&format!(r#"{{"id":{init_id},"result":{{}}}}"#));
        let ids: Vec<u64> = outs
            .iter()
            .filter_map(|o| match o {
                Out::Send(v)
                    if v.get("method")
                        .and_then(Value::as_str)
                        .is_some_and(|m| m.starts_with("account/")) =>
                {
                    v["id"].as_u64()
                }
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(
            t.on_line(&format!(
                r#"{{"id":{},"result":{{"account":{{"email":"a@b"}}}}}}"#,
                ids[0]
            ))
            .is_empty()
        );
        assert!(
            t.on_line(&format!(
                r#"{{"id":{},"error":{{"message":"no"}}}}"#,
                ids[1]
            ))
            .is_empty()
        );
        let out = t.on_line(&format!(
            r#"{{"id":{},"result":{{"summary":{{}}}}}}"#,
            ids[2]
        ));
        let [
            Out::Reply {
                query,
                result: Ok(v),
            },
        ] = out.as_slice()
        else {
            panic!("expected the reply");
        };
        assert_eq!(*query, q);
        assert_eq!(v["account"]["account"]["email"], "a@b");
        assert!(v["rateLimits"].is_null());
        assert_eq!(v["session"]["cwd"], "/w");
    }

    fn sent<'a>(out: &'a [Out], method: &str) -> Option<&'a Value> {
        out.iter().find_map(|o| match o {
            Out::Send(v) if v["method"] == method => Some(v),
            _ => None,
        })
    }

    #[test]
    fn steering_waits_for_the_turn_id_then_goes_in() {
        let mut t = Translator::new("/w".into(), None, None, None);
        t.thread_id = Some("T1".into());
        let out = t.start_turn_with("first", &[]);
        let start = sent(&out, "turn/start").expect("turn/start")["id"]
            .as_u64()
            .unwrap();
        // Codex hasn't said which turn yet: the message waits.
        assert!(t.steer_with("second", &[]).is_empty());
        let out = t.on_line(&format!(
            r#"{{"id":{start},"result":{{"turn":{{"id":"U1"}}}}}}"#
        ));
        let steer = sent(&out, "turn/steer").expect("turn/steer");
        assert_eq!(steer["params"]["expectedTurnId"], "U1");
        assert_eq!(steer["params"]["input"][0]["text"], "second");
        // No new turn for it.
        assert!(
            !out.iter()
                .any(|o| matches!(o, Out::Event(HarnessEvent::TurnStarted)))
        );
        // With the id known, it goes straight in.
        let out = t.steer_with("third", &[]);
        assert!(sent(&out, "turn/steer").is_some());
    }

    #[test]
    fn a_steer_that_misses_the_turn_starts_the_next() {
        let mut t = Translator::new("/w".into(), None, None, None);
        t.thread_id = Some("T1".into());
        t.turn_id = Some("U1".into());
        let out = t.steer_with("late", &[]);
        let id = sent(&out, "turn/steer").unwrap()["id"].as_u64().unwrap();
        t.turn_id = None; // the turn ended meanwhile
        let out = t.on_line(&format!(
            r#"{{"id":{id},"error":{{"message":"no active turn"}}}}"#
        ));
        assert!(matches!(out[0], Out::Event(HarnessEvent::TurnStarted)));
        let start = sent(&out, "turn/start").expect("a new turn");
        assert_eq!(start["params"]["input"][0]["text"], "late");
    }

    #[test]
    fn commands_and_skills() {
        let mut t = Translator::new("/w".into(), None, None, None);
        let review = t.turn_start_request("/review", &[], "T1");
        assert_eq!(review["method"], "review/start");
        assert_eq!(review["params"]["target"]["type"], "uncommittedChanges");
        let custom = t.turn_start_request("/review look at auth", &[], "T1");
        assert_eq!(custom["params"]["target"]["instructions"], "look at auth");
        assert_eq!(
            t.turn_start_request("/compact", &[], "T1")["method"],
            "thread/compact/start"
        );
        let init = t.turn_start_request("/init", &[], "T1");
        assert!(
            init["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("AGENTS.md")
        );
        // Skills arrive as commands that insert `$name`, and a mention sends the skill.
        let id = t.request(Pending::Skills, "skills/list", json!({}))["id"]
            .as_u64()
            .unwrap();
        let out = t.on_line(&format!(
            r#"{{"id":{id},"result":{{"data":[{{"cwd":"/w","errors":[],"skills":[{{"name":"pdf","path":"/s/pdf/SKILL.md","description":"PDFs","enabled":true,"scope":"user"}}]}}]}}}}"#
        ));
        let [Out::Event(HarnessEvent::CommandsAvailable(cmds))] = out.as_slice() else {
            panic!("expected the commands");
        };
        let pdf = cmds.iter().find(|c| c.name == "pdf").unwrap();
        assert_eq!(pdf.insert, "$pdf");
        assert!(cmds.iter().any(|c| c.name == "review" && c.builtin));
        let turn = t.turn_start_request("use $pdf on this", &[], "T1");
        assert_eq!(turn["params"]["input"][1]["type"], "skill");
        assert_eq!(turn["params"]["input"][1]["path"], "/s/pdf/SKILL.md");
    }

    #[test]
    fn goals_and_codex_started_turns() {
        let mut t = Translator::new("/w".into(), None, None, None);
        t.thread_id = Some("T1".into());
        let sent = |out: &[Out]| -> Vec<(String, Value)> {
            out.iter()
                .filter_map(|o| match o {
                    Out::Send(v) => {
                        Some((v["method"].as_str().unwrap_or("").to_owned(), v.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        // A new goal: cleared, set paused, then our turn; active once it runs.
        let out = t.start_turn_with("/goal make the tests pass", &[]);
        let s = sent(&out);
        let methods: Vec<&str> = s.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            ["thread/goal/clear", "thread/goal/set", "turn/start"]
        );
        assert_eq!(s[1].1["params"]["status"], "paused");
        let turn_id = s[2].1["id"].as_u64().unwrap();
        let out = t.on_line(&format!(
            r#"{{"id":{turn_id},"result":{{"turn":{{"id":"U1"}}}}}}"#
        ));
        assert_eq!(sent(&out)[0].1["params"]["status"], "active");
        // Our own turn starting isn't reported again; one Codex starts is.
        assert!(
            events(&t.on_line(r#"{"method":"turn/started","params":{"turn":{"id":"U1"}}}"#))
                .is_empty()
        );
        let out = t.on_line(r#"{"method":"turn/started","params":{"turn":{"id":"U2"}}}"#);
        assert!(matches!(events(&out)[0], HarnessEvent::TurnStarted));
        // `/goal pause` answers itself and ends the turn.
        let out = t.start_turn_with("/goal pause", &[]);
        let id = sent(&out)[0].1["id"].as_u64().unwrap();
        let out = t.on_line(&format!(r#"{{"id":{id},"result":{{"goal":null}}}}"#));
        let ev = events(&out);
        assert!(matches!(
            ev.last(),
            Some(HarnessEvent::TurnCompleted { .. })
        ));
        assert!(ev.iter().any(|e| matches!(e, HarnessEvent::ItemCompleted { body: Some(ItemBody::AssistantMessage { text }), .. } if text == "Goal paused.")));
    }

    #[test]
    fn unsupported_server_requests_are_answered() {
        let mut t = Translator::new("/w".into(), None, None, None);
        let out = t.on_line(r#"{"id":7,"method":"account/chatgptAuthTokens/refresh","params":{}}"#);
        let Out::Send(v) = &out[0] else { panic!() };
        assert_eq!(v["id"], 7);
        assert!(v.get("error").is_some());
    }
}
