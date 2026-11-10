//! Domain model shared over the wire (PLAN §1.4).

use crate::ids::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root_path: String,
    pub created_at_ms: i64,
    /// What new threads in this project start with (unset = the global
    /// defaults).
    #[serde(default, skip_serializing_if = "ProjectDefaults::is_empty")]
    pub defaults: ProjectDefaults,
}

/// A project's own defaults for new threads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectDefaults {
    /// The agent new threads use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<HarnessId>,
    /// Its model (for `agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// `Some(true)`: a new worktree each; `Some(false)`: the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<bool>,
}

impl ProjectDefaults {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A thread whose conversation mentions what was searched for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchHit {
    pub thread_id: ThreadId,
    /// The first message that matches.
    pub item_id: ItemId,
    /// Whether it was the user's message (else the agent's).
    pub from_user: bool,
    /// A line of that message around the match.
    pub snippet: String,
    /// Byte range of the match within `snippet`.
    pub start: usize,
    pub end: usize,
    /// How many messages in the thread match.
    pub count: u32,
}

/// A slash command the agent accepts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlashCommandInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What to type after it, e.g. `<model>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub argument_hint: String,
    /// One of the agent's own commands (vs. a skill or plugin).
    #[serde(default)]
    pub builtin: bool,
    /// What picking it puts in the composer when not `/name` (e.g. a
    /// Codex skill's `$name`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub insert: String,
}

/// The commands Agentloft runs for Codex (through its app-server or the
/// app itself); its skills come on top, once its session reports them.
pub fn codex_builtin_commands() -> Vec<SlashCommandInfo> {
    let c = |name: &str, hint: &str, description: &str| SlashCommandInfo {
        name: name.into(),
        description: description.into(),
        argument_hint: hint.into(),
        builtin: true,
        insert: String::new(),
    };
    vec![
        c(
            "review",
            "[instructions]",
            "Review the uncommitted changes (or follow your instructions)",
        ),
        c(
            "compact",
            "",
            "Free up context by summarizing the conversation so far",
        ),
        c(
            "goal",
            "[objective | pause | resume | clear]",
            "Set a goal Codex keeps working toward until it is done (no argument: show it)",
        ),
        c(
            "init",
            "",
            "Create an AGENTS.md with instructions for this repository",
        ),
        c("status", "", "Account, limits and this thread's token use"),
        c("model", "", "Choose the model and reasoning effort"),
        c("diff", "", "Show this thread's changes"),
        c("new", "", "Start a new thread"),
    ]
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
    /// The same commands with what they do, when the agent says so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_info: Vec<SlashCommandInfo>,
    /// When the first message was sent. Unstarted threads stay out of the
    /// sidebar unless they hold a draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// Unsent composer text, kept across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    /// Background work the agent started that hasn't finished (what each
    /// is): with no turn running, the thread is waiting on these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<String>,
}

impl Thread {
    /// Idle, but waiting on background work it started.
    pub fn is_waiting(&self) -> bool {
        !self.waiting_on.is_empty() && matches!(self.status, ThreadStatus::Idle)
    }
}

impl Thread {
    /// Whether a message was ever sent (threads from before
    /// `started_at_ms` have a native session by then).
    pub fn is_started(&self) -> bool {
        self.started_at_ms.is_some() || self.native_session_id.is_some()
    }

    /// The draft, if it has anything in it.
    pub fn draft_text(&self) -> Option<&str> {
        self.draft.as_deref().filter(|d| !d.trim().is_empty())
    }
}

/// Where a fork came from (ADR-0011).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadParent {
    pub thread_id: ThreadId,
    /// The parent's last turn at the fork point.
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    pub kind: ForkKind,
    /// Shown as a thread of its own in the sidebar, not under its parent
    /// (still linked: who started it can follow it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForkKind {
    /// The provider's own fork: the full conversation carries over.
    Native,
    /// A different provider: the new thread starts from a handoff digest.
    Handoff,
    /// Started by another thread's agent (orchestration): a fresh
    /// conversation with its own prompt.
    Spawned,
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
    /// named after it, starting from `base` (default: the main branch).
    NewWorktree {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<WorktreeBase>,
    },
    /// A git worktree on its own `agentloft/<slug>` branch.
    Worktree {
        path: String,
        branch: String,
        /// Branch (or commit) it was created from; merge-back target.
        base: String,
    },
}

/// Where a new worktree starts: a branch, as the local copy has it or (with
/// `remote`) as fetched from that remote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeBase {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// A repository's branches (`ListBranches`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchList {
    /// Local branches, most recently committed first.
    pub local: Vec<String>,
    /// Remote-tracking branches, most recent first.
    pub remote: Vec<RemoteBranch>,
    /// The main branch (`origin/HEAD`, else `main`/`master`).
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    pub branch: String,
}

impl ThreadWorkspace {
    pub fn is_new_worktree(&self) -> bool {
        matches!(self, ThreadWorkspace::NewWorktree { .. })
    }

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
    /// The agent's own marker for the end of this turn (Claude's last
    /// assistant message uuid, Codex's turn id), to fork from it natively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_turn: Option<String>,
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
    /// When the item began (older items don't have it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
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
