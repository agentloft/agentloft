//! Domain model shared over the wire (PLAN §1.4).

use crate::ids::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root_path: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub harness: HarnessId,
    /// Working directory the agent runs in (repo root or worktree).
    pub cwd: String,
    pub model: Option<String>,
    /// `None` = the harness's own default (decided; PLAN §2.7).
    pub permission_mode: Option<PermissionMode>,
    /// Reasoning effort (`low` … `max`); `None` = the model's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Provider session id (Claude session UUID, Codex thread id, …).
    pub native_session_id: Option<String>,
    pub status: ThreadStatus,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Where the agent works: the project checkout or its own worktree.
    #[serde(default)]
    pub workspace: ThreadWorkspace,
    /// Set on forks: the thread this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ThreadParent>,
    /// Slash commands the agent reported (composer completions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_commands: Vec<String>,
}

/// Where a fork came from (ADR-0011).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadParent {
    pub thread_id: ThreadId,
    /// The parent's last turn at the fork point.
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub kind: ForkKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForkKind {
    /// The provider's own fork: the full conversation carries over.
    Native,
    /// A different provider: the new thread starts from a handoff digest.
    Handoff,
}

/// An attachment stored by the daemon (`~/.agentloft/attachments`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentRef {
    pub name: String,
    pub mime: String,
    pub path: String,
}

/// A thread's workspace (ADR-0008, PLAN §5.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadWorkspace {
    /// The project checkout itself.
    #[default]
    Local,
    /// A worktree is created when the first message is sent, on a branch
    /// named after it.
    NewWorktree,
    /// A git worktree on its own `agentloft/<slug>` branch.
    Worktree {
        path: String,
        branch: String,
        /// Branch (or commit) it was created from; merge-back target.
        base: String,
    },
}

impl ThreadWorkspace {
    pub fn branch(&self) -> Option<&str> {
        match self {
            ThreadWorkspace::Worktree { branch, .. } => Some(branch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Running,
    WaitingApproval,
    Error,
    Archived,
}

/// Agentloft's canonical permission modes (PLAN §2.7).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    AskAll,
    AcceptEdits,
    Auto,
    FullAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub ordinal: u32,
    pub status: TurnStatus,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub usage: Option<Usage>,
    pub error: Option<String>,
    /// Git checkpoint commits captured before/after the turn (ADR-0008).
    #[serde(default)]
    pub checkpoint_before: Option<String>,
    #[serde(default)]
    pub checkpoint_after: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}

impl TurnStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, TurnStatus::Running)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub thread_id: ThreadId,
    pub turn_id: Option<TurnId>,
    /// Position within the thread timeline.
    pub ordinal: u64,
    pub status: ItemStatus,
    pub body: ItemBody,
    pub updated_at_ms: i64,
    /// The subagent (Task/Agent tool call) this item belongs to, if any.
    #[serde(default)]
    pub parent_id: Option<ItemId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemBody {
    UserMessage {
        text: String,
        /// Images sent with the message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<AttachmentRef>,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        tool: String,
        tool_kind: ToolKind,
        title: String,
        input: serde_json::Value,
        output: Option<String>,
        is_error: bool,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    Error {
        text: String,
    },
}

impl ItemBody {
    /// Mutable access to the streamed text of this item, if it has one.
    pub fn stream_text_mut(&mut self, channel: crate::DeltaChannel) -> Option<&mut String> {
        use crate::DeltaChannel::*;
        match (self, channel) {
            (ItemBody::AssistantMessage { text }, Text) => Some(text),
            (ItemBody::Reasoning { text }, Reasoning) => Some(text),
            (ItemBody::ToolCall { output, .. }, Output) => Some(output.get_or_insert_default()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Command,
    FileChange,
    FileRead,
    Search,
    WebFetch,
    Mcp,
    Subagent,
    Todo,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
}

/// An approval or question the harness is waiting on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRequest {
    pub id: RequestId,
    pub thread_id: ThreadId,
    pub turn_id: Option<TurnId>,
    pub item_id: Option<ItemId>,
    pub kind: RequestKind,
    pub title: String,
    pub detail: serde_json::Value,
    pub options: Vec<DecisionKind>,
    pub status: RequestStatus,
    pub decision: Option<DecisionKind>,
    pub opened_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    ToolApproval,
    FileChangeApproval,
    Question,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Open,
    Resolved,
    Expired,
}

/// Canonical approval decisions (PLAN §2.7 mapping table).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    AllowOnce,
    AllowSession,
    Deny,
    DenyAndStop,
}

impl DecisionKind {
    pub fn label(self) -> &'static str {
        match self {
            DecisionKind::AllowOnce => "Allow",
            DecisionKind::AllowSession => "Allow for session",
            DecisionKind::Deny => "Deny",
            DecisionKind::DenyAndStop => "Deny & stop",
        }
    }
}
