//! The harness abstraction (PLAN §2.1–2.3).
//!
//! A [`Harness`] opens [`SessionHandle`]s. A session is an actor: the core sends
//! [`SessionCommand`]s and receives canonical [`HarnessEvent`]s. Adapters
//! (Claude, Codex, ACP, fake) translate their native protocol into these.

pub mod fake;
pub mod process;

use loft_protocol::{
    DecisionKind, DeltaChannel, HarnessId, ItemBody, ItemStatus, NoticeLevel, PermissionMode,
    RequestKind, TurnStatus, Usage,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Everything needed to start (or resume) one provider session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// For logging only.
    pub thread_label: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Reasoning effort; `None` = the model's default.
    pub effort: Option<String>,
    /// `None` = don't pass any mode; the harness's own default applies.
    pub permission_mode: Option<PermissionMode>,
    /// Resume this native session.
    pub resume: Option<String>,
    /// Pre-assigned native session id for a brand-new session (Claude).
    pub new_session_id: Option<String>,
    /// Start as a native fork of this session (ADR-0011); ignored when
    /// `resume` is set.
    pub fork_from: Option<String>,
    /// With `fork_from`: fork through this turn marker only.
    pub fork_at: Option<String>,
    /// Environment for the child (login-shell capture).
    pub env: Arc<HashMap<String, String>>,
    /// Resolved binary path, or `None` to resolve via `env["PATH"]`.
    pub binary: Option<PathBuf>,
    pub extra_args: Vec<String>,
    /// MCP servers to attach to the session (e.g. the Agentloft server).
    pub mcp_servers: Vec<McpServer>,
    /// Tools the harness may use without asking (e.g. `mcp__agentloft`).
    pub allowed_tools: Vec<String>,
}

/// A stdio MCP server to attach to an agent session.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TurnInput {
    pub text: String,
    pub images: Vec<ImageInput>,
}

/// An image attached to a prompt: a file the daemon stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageInput {
    pub path: std::path::PathBuf,
    pub mime: String,
}

#[derive(Debug)]
pub enum SessionCommand {
    StartTurn(TurnInput),
    /// Add to the running turn without stopping it (agents with
    /// `Capabilities::steer`).
    Steer(TurnInput),
    Interrupt,
    /// Stop the subagent started by the tool call `native_id` (agents that
    /// can't stop one subagent ignore it).
    StopSubagent(String),
    /// Ask the agent for a report; agents that can't answer drop `reply`.
    Query {
        kind: loft_protocol::AgentInfoKind,
        reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Respond {
        native_request_id: String,
        decision: DecisionKind,
        /// Answers to a question, or feedback on a plan.
        reply: Option<loft_protocol::RequestReply>,
    },
    SetModel(Option<String>),
    SetEffort(Option<String>),
    SetPermissionMode(Option<PermissionMode>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

/// A turn stopped by a usage limit, and when that limit resets if the
/// agent said (`HarnessEvent::LimitReached`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitHit {
    pub resets_at_ms: Option<i64>,
}

/// Canonical events every adapter emits (PLAN §2.2).
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    SessionReady {
        native_session_id: String,
        model: Option<String>,
        slash_commands: Vec<String>,
        /// Models the agent offers, when it says so at session start (ACP
        /// `configOptions`); feeds the model picker.
        models: Vec<loft_protocol::ModelInfo>,
    },
    /// The agent's slash commands with what they do (Claude's initialize
    /// response).
    CommandsAvailable(Vec<loft_protocol::SlashCommandInfo>),
    /// Work the agent started that is still going (background shells,
    /// subagents): what each is. Empty once all have finished.
    BackgroundTasks(Vec<String>),
    TurnStarted,
    ItemStarted {
        native_id: String,
        body: ItemBody,
        /// Native id of the subagent tool call this item belongs to.
        parent: Option<String>,
    },
    ItemDelta {
        native_id: String,
        channel: DeltaChannel,
        text: String,
    },
    /// A tool call's input as it stands while the agent is still writing
    /// it (`partial`), or once it is whole. Only for calls whose item was
    /// started before their input was known (a view being written, ADR-0018).
    ToolInput {
        native_id: String,
        input: serde_json::Value,
        partial: bool,
    },
    /// What a tool call gave back while the work it started goes on: a
    /// subagent launched in the background answers at once with a receipt.
    /// The item stays in progress until its `ItemCompleted`.
    ToolOutput {
        native_id: String,
        result: ToolResult,
    },
    ItemCompleted {
        native_id: String,
        /// Replace the body entirely (e.g. final full text). `None` keeps the
        /// accumulated body.
        body: Option<ItemBody>,
        tool_result: Option<ToolResult>,
        status: ItemStatus,
    },
    RequestOpened {
        native_id: String,
        kind: RequestKind,
        title: String,
        detail: serde_json::Value,
        options: Vec<DecisionKind>,
        /// Native id of the item (tool call) this request is about.
        related_item: Option<String>,
    },
    RequestResolved {
        native_id: String,
    },
    /// How full the agent's context is after its latest step.
    ContextUsed(loft_protocol::ContextUse),
    /// The agent's marker for the turn so far (see `Turn::native_turn`).
    TurnMarker {
        native_turn: String,
    },
    /// The usage limit of the user's plan is what ends this turn: sent just
    /// before the `TurnCompleted` that fails it.
    LimitReached(LimitHit),
    TurnCompleted {
        status: TurnStatus,
        usage: Option<Usage>,
        error: Option<String>,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    /// The session's process went away. No more events follow.
    Exited {
        code: Option<i32>,
        stderr_tail: String,
    },
}

pub struct SessionHandle {
    pub commands: mpsc::UnboundedSender<SessionCommand>,
    pub events: mpsc::UnboundedReceiver<HarnessEvent>,
    /// OS pid of the agent process (process-group leader), if any.
    pub pid: Option<u32>,
}

impl SessionHandle {
    /// A connected pair: the adapter keeps the `SessionEnds`, the core gets
    /// the handle.
    pub fn pair() -> (SessionHandle, SessionEnds) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel();
        (
            SessionHandle {
                commands: cmd_tx,
                events: evt_rx,
                pid: None,
            },
            SessionEnds {
                commands: cmd_rx,
                events: evt_tx,
            },
        )
    }
}

/// The adapter side of a session.
pub struct SessionEnds {
    pub commands: mpsc::UnboundedReceiver<SessionCommand>,
    pub events: mpsc::UnboundedSender<HarnessEvent>,
}

impl SessionEnds {
    pub fn emit(&self, event: HarnessEvent) {
        let _ = self.events.send(event);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// The client may choose the native session id up front (Claude).
    pub client_session_id: bool,
    pub steer: bool,
    pub live_model_switch: bool,
    pub live_mode_switch: bool,
    /// Effort changes reach a running session; otherwise the core restarts
    /// the session (resuming it) before the next turn.
    pub live_effort_switch: bool,
    pub native_fork: bool,
    /// A `/compact` prompt compacts the conversation (a turn of its own).
    pub compact: bool,
}

pub trait Harness: Send + Sync + 'static {
    fn id(&self) -> HarnessId;
    fn capabilities(&self) -> Capabilities;
    /// Spawn the session. Must be called within a tokio runtime.
    fn open_session(&self, cfg: SessionConfig) -> anyhow::Result<SessionHandle>;
    /// Models to offer in the picker. Blocking (may start the CLI briefly);
    /// call off the async runtime. `env` is the login environment.
    fn list_models(
        &self,
        _env: &HashMap<String, String>,
        _binary: Option<PathBuf>,
    ) -> anyhow::Result<Vec<loft_protocol::ModelInfo>> {
        Ok(Vec::new())
    }
    /// Slash commands and skills for `cwd`, without a session. Blocking
    /// (may start the CLI briefly). Empty = they come with the session.
    fn list_commands(
        &self,
        _env: &HashMap<String, String>,
        _binary: Option<PathBuf>,
        _cwd: &str,
    ) -> anyhow::Result<Vec<loft_protocol::SlashCommandInfo>> {
        Ok(Vec::new())
    }
}
