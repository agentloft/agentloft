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
    /// An image chosen as its icon (else its favicon, else initials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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
    /// The last turn was stopped by the agent's usage limit. Gone when the
    /// next turn starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitStop>,
    /// Started by this automation (one of its runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<AutomationId>,
    /// The pull request for its branch, as GitHub last described it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequest>,
    /// Everything on its worktree's branch is in the base branch (merged
    /// back, or its pull request merged) and nothing is left uncommitted:
    /// the worktree can be cleaned up without losing anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub landed: bool,
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

    /// A copy without the agent's commands, for whoever lists or labels
    /// threads. The commands are most of a thread (a hundred or so, each
    /// with a description: tens of kilobytes), and a list drawn on every
    /// frame has no use for them.
    pub fn without_commands(&self) -> Thread {
        // Every field by name: a new one is a compile error here, not a
        // value that goes missing from the copy.
        let Thread {
            id,
            project_id,
            title,
            harness,
            cwd,
            model,
            permission_mode,
            effort,
            native_session_id,
            status,
            created_at_ms,
            updated_at_ms,
            workspace,
            parent,
            slash_commands: _,
            command_info: _,
            started_at_ms,
            draft,
            waiting_on,
            limit,
            automation_id,
            pull_request,
            landed,
        } = self;
        Thread {
            id: id.clone(),
            project_id: project_id.clone(),
            title: title.clone(),
            harness: harness.clone(),
            cwd: cwd.clone(),
            model: model.clone(),
            permission_mode: *permission_mode,
            effort: effort.clone(),
            native_session_id: native_session_id.clone(),
            status: *status,
            created_at_ms: *created_at_ms,
            updated_at_ms: *updated_at_ms,
            workspace: workspace.clone(),
            parent: parent.clone(),
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: *started_at_ms,
            draft: draft.clone(),
            waiting_on: waiting_on.clone(),
            limit: *limit,
            automation_id: automation_id.clone(),
            pull_request: pull_request.clone(),
            landed: *landed,
        }
    }
}

/// A thread's pull request (kept small: a thread is logged whole on every
/// change).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: PullRequestState,
    /// How its checks stand (all zero: none reported).
    #[serde(default, skip_serializing_if = "PullRequestChecks::is_empty")]
    pub checks: PullRequestChecks,
    /// What reviewers decided, when the repository asks for reviews or
    /// someone has reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewDecision>,
    /// The commit its branch was at on GitHub (what a merge took).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// When GitHub last said something new about it. A look that finds it
    /// unchanged is not recorded.
    #[serde(default)]
    pub refreshed_at_ms: i64,
}

impl PullRequest {
    /// Still open (a draft is): its checks and reviews can change.
    pub fn is_open(&self) -> bool {
        matches!(self.state, PullRequestState::Open | PullRequestState::Draft)
    }

    /// The same pull request in the same state, whenever each was fetched.
    pub fn same_as(&self, other: &PullRequest) -> bool {
        // Every field by name: a new one must be compared (or left out) here.
        let PullRequest {
            number,
            url,
            title,
            state,
            checks,
            review,
            head,
            refreshed_at_ms: _,
        } = self;
        *number == other.number
            && *url == other.url
            && *title == other.title
            && *state == other.state
            && *checks == other.checks
            && *review == other.review
            && *head == other.head
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// A pull request's checks, counted by how each stands. Skipped checks are
/// left out.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequestChecks {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub passing: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failing: u32,
    /// Queued or running.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pending: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl PullRequestChecks {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// All of them in one word: failing if any failed, else running if any
    /// is still going, else passing. `None`: there are no checks.
    pub fn rollup(&self) -> Option<ChecksState> {
        if self.failing > 0 {
            Some(ChecksState::Failing)
        } else if self.pending > 0 {
            Some(ChecksState::Pending)
        } else if self.passing > 0 {
            Some(ChecksState::Passing)
        } else {
            None
        }
    }

    /// "3 passing, 1 failing, 2 running" (`None`: there are no checks).
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = [
            (self.passing, "passing"),
            (self.failing, "failing"),
            (self.pending, "running"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksState {
    Passing,
    Failing,
    Pending,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// A turn the agent's usage limit stopped (`Thread::limit`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LimitStop {
    /// When the limit resets, if the agent said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    /// Carry on by itself when it resets (`Request::ResumeAtReset`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
}

/// A message waiting in a thread's queue: sent, in order, as each turn
/// completes. The daemon holds it, so it outlives the window that typed it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedMessage {
    /// Stays the same while others are sent or moved around it.
    pub id: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
    pub queued_at_ms: i64,
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
    /// Its worktree was cleaned up once its work was in `base`. The next
    /// message makes a new one at `path` (so the agent's session, kept per
    /// folder, carries on) on a new branch from `base`.
    Released {
        path: String,
        /// The branch it had.
        branch: String,
        base: String,
        /// Where that branch was when it was deleted (`None`: it was kept),
        /// to bring it back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tip: Option<String>,
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

/// A worktree folder or an Agentloft branch that no active thread works
/// in (`WorktreeLeftovers`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeLeftover {
    pub project_id: ProjectId,
    /// The worktree's folder, if it still has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Its branch (`agentloft/<slug>`), if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The archived thread it belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// Everything on it is in the main branch (and the folder, if any, has
    /// nothing uncommitted): removing it loses nothing.
    pub landed: bool,
    /// Commits on the branch that the main branch doesn't have.
    #[serde(default)]
    pub ahead: u32,
    /// Uncommitted changes in the folder.
    #[serde(default)]
    pub dirty: bool,
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
    /// The files the turn changed (between its checkpoints), worked out
    /// once when it ends; older turns get it the first time it's asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<crate::DiffFileStat>>,
    /// How full the agent's context was at its latest step in this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUse>,
    /// The model that ran the turn, as the agent named it (older turns
    /// don't have it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Tokens in the agent's context, and the most it holds when it says.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUse {
    pub tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
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
    /// What the user answered (questions) or said (a plan sent back).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<RequestReply>,
}

impl RuntimeRequest {
    /// A question request's questions (`detail.questions`).
    pub fn questions(&self) -> Vec<Question> {
        self.detail
            .get("questions")
            .and_then(|q| serde_json::from_value(q.clone()).ok())
            .unwrap_or_default()
    }

    /// A plan request's plan, as Markdown (`detail.plan`).
    pub fn plan(&self) -> Option<&str> {
        self.detail.get("plan").and_then(|p| p.as_str())
    }
}

/// One question an agent asks (`RequestKind::Question`; the request's
/// `detail` is `{"questions": [Question…]}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Question {
    /// What the answer is keyed by when it goes back to the agent.
    pub id: String,
    /// A short label for it (a tab, a chip).
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// More than one option may be picked.
    #[serde(default)]
    pub multi_select: bool,
    /// The user may type an answer of their own.
    #[serde(default)]
    pub allow_other: bool,
    /// The typed answer is a secret (hide it as it is typed).
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// What goes back with a decision beyond allow/deny.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RequestReply {
    /// Answers to a question request, one per question answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<QuestionAnswer>,
    /// Words for the agent (a plan sent back: what to change).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QuestionAnswer {
    /// The question's `id`.
    pub id: String,
    /// Labels picked and/or text typed.
    pub answers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    ToolApproval,
    FileChangeApproval,
    /// The agent asks the user something (`detail.questions`).
    Question,
    /// The agent has a plan and asks to go ahead with it (`detail.plan`).
    /// Allow = approve, AllowSession = approve and accept edits, Deny =
    /// keep planning (with `RequestReply::message` as feedback).
    Plan,
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
