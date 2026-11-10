//! Wire types shared by `agentloftd` and its clients (PLAN §1.6).
//!
//! Everything here is plain serde data: no I/O, no async, no GPUI. Frames are
//! length-delimited JSON (`u32` big-endian length prefix) carrying [`ClientMsg`]
//! and [`ServerMsg`].

pub mod ids;
pub mod model;
pub mod pages;
pub mod workbench;

pub use ids::*;
pub use model::*;
pub use pages::*;
pub use workbench::*;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible wire change. The daemon accepts `N` and `N-1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum frame size accepted on the socket (16 MiB).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Client → daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(Hello),
    Request {
        id: u64,
        body: Request,
    },
    Subscribe {
        sub_id: u64,
        topic: Topic,
    },
    Unsubscribe {
        sub_id: u64,
    },
    /// Reply to a [`ServerMsg::ClientCall`].
    ClientCallResult {
        call_id: u64,
        result: Result<serde_json::Value, ErrorBody>,
    },
    /// This viewer has shown [`ServerMsg::PageFrame`] `seq` and is ready for
    /// the next. One frame is in flight per subscription, so a slow link
    /// gets fewer frames instead of a growing queue.
    PageFrameAck {
        sub_id: u64,
        seq: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_kind: ClientKind,
    pub client_build: String,
    /// Optional client capabilities, e.g. `"browser"` (can run browser calls).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    LocalUi,
    RemoteUi,
    McpBridge,
    Cli,
    Test,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    AddProject {
        path: String,
        #[serde(default)]
        name: Option<String>,
    },
    RenameProject {
        project_id: ProjectId,
        name: String,
    },
    /// Settings for agents: current values, settable keys, themes
    /// (`Response::Json`).
    GetSettings,
    /// Set one known setting (`ui.theme`, `harness.codex.default_model`…);
    /// `null` resets it (`Response::Text`).
    ChangeSetting {
        key: String,
        value: serde_json::Value,
    },
    /// Save a colour theme (its TOML) and optionally switch to it. Saved
    /// under `id` (`custom:<name>`) when given, else under a name made from
    /// the theme's own.
    SaveTheme {
        toml: String,
        #[serde(default)]
        select: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// A project's icon: an image file, or `None` for automatic (answers
    /// `Response::Project`).
    SetProjectIcon {
        project_id: ProjectId,
        #[serde(default)]
        icon: Option<String>,
    },
    /// What new threads in a project start with (answers
    /// `Response::Project`).
    SetProjectDefaults {
        project_id: ProjectId,
        defaults: ProjectDefaults,
    },
    /// Remove a project from Agentloft: its threads are deleted (worktrees
    /// and checkpoints too); the folder on disk is left alone.
    RemoveProject {
        project_id: ProjectId,
    },
    /// A new project named `name`: a git repository (README, first commit)
    /// in a folder named after it, under `parent` (default: Agentloft's
    /// projects folder), optionally published as a private GitHub
    /// repository. Answers `Response::Project`.
    CreateProject {
        #[serde(default)]
        parent: Option<String>,
        name: String,
        #[serde(default)]
        github: bool,
    },
    /// The GitHub account `gh` is signed in to (`Response::Text`, empty if
    /// none).
    GitHubLogin,
    /// A project cloned from `source` (a git URL, or GitHub `owner/repo`
    /// with `github`) into `parent` (answers `Response::Project`).
    CloneProject {
        parent: String,
        source: String,
        #[serde(default)]
        github: bool,
    },
    /// A thread without a project (a chat): kept in the built-in "Chats"
    /// project, working in an empty folder of its own. Answers
    /// `Response::Thread`.
    CreateChat {
        harness: HarnessId,
    },
    CreateThread {
        project_id: ProjectId,
        harness: HarnessId,
        #[serde(default)]
        title: Option<String>,
    },
    SendMessage {
        thread_id: ThreadId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// A message for the running turn: the agent takes it in at its next
    /// step, without stopping. With no turn running, it starts one.
    Steer {
        thread_id: ThreadId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    Interrupt {
        thread_id: ThreadId,
    },
    /// Ask the thread's agent for a report (`Response::AgentInfo`): its
    /// context window, or plan usage and limits.
    AgentInfo {
        thread_id: ThreadId,
        kind: AgentInfoKind,
    },
    /// Stop one running subagent (its Agent tool call item), leaving the
    /// rest of the turn alone.
    StopSubagent {
        thread_id: ThreadId,
        item_id: ItemId,
    },
    RespondRequest {
        thread_id: ThreadId,
        request_id: RequestId,
        decision: DecisionKind,
        /// Answers to a question, or feedback on a plan.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<RequestReply>,
    },
    SetPermissionMode {
        thread_id: ThreadId,
        mode: Option<PermissionMode>,
    },
    /// Switch the model (`None` = the harness default). Applies from the
    /// next turn (Claude switches live).
    SetModel {
        thread_id: ThreadId,
        model: Option<String>,
    },
    /// Set the reasoning effort (`None` = the model's default). Codex applies
    /// it from the next turn; Claude restarts its process between turns.
    SetEffort {
        thread_id: ThreadId,
        effort: Option<String>,
    },
    /// Change a thread's harness; only allowed before its first message.
    SetThreadHarness {
        thread_id: ThreadId,
        harness: HarnessId,
    },
    /// Work in the project checkout or in a new worktree (created on the
    /// first message); only allowed before the first message.
    SetThreadWorkspace {
        thread_id: ThreadId,
        worktree: bool,
        /// Where the worktree starts; `None` = the main branch.
        #[serde(default)]
        base: Option<WorktreeBase>,
    },
    /// A project's branches, for choosing where a worktree starts.
    ListBranches {
        project_id: ProjectId,
    },
    /// Squash-merge a worktree thread's branch into its base branch in the
    /// project checkout (uncommitted work is committed to the branch first).
    /// Answers `Response::Text` with a summary.
    MergeThread {
        thread_id: ThreadId,
        /// Commit message; defaults to the thread title.
        #[serde(default)]
        message: Option<String>,
    },
    /// Fork a thread (ADR-0011). The same harness forks natively; another
    /// harness gets a handoff digest back as a `draft` first message for the
    /// user to review. The fork gets its own worktree from the parent's
    /// current state unless `share_workspace` (or not a git repo).
    ForkThread {
        thread_id: ThreadId,
        /// Harness for the fork; defaults to the parent's.
        #[serde(default)]
        harness: Option<HarnessId>,
        #[serde(default)]
        share_workspace: bool,
        /// Fork from the end of this turn instead of the latest one.
        #[serde(default)]
        at_turn: Option<TurnId>,
    },
    /// Restore the thread's files to how they were before `turn_id` (undoing
    /// that turn and every later one). The conversation is kept; the agent
    /// is told on its next turn. Answers `Response::Text` with a summary.
    RevertTurn {
        thread_id: ThreadId,
        turn_id: TurnId,
    },
    RenameThread {
        thread_id: ThreadId,
        title: String,
    },
    /// One self-contained question to an agent, without tools or a thread
    /// (e.g. "design a colour theme"). Answers `Response::Text`.
    AskOnce {
        prompt: String,
        /// Defaults to Claude Code.
        #[serde(default)]
        harness: Option<HarnessId>,
        /// The agent's default when unset.
        #[serde(default)]
        model: Option<String>,
    },
    /// Save the composer's unsent text (empty clears it).
    SetDraft {
        thread_id: ThreadId,
        text: String,
    },
    /// Bring an archived thread back (its worktree is re-created from the
    /// kept branch).
    UnarchiveThread {
        thread_id: ThreadId,
    },
    /// Push a worktree thread's branch and open a GitHub pull request into
    /// its base with `gh`. Answers `Response::Text` with the PR URL.
    CreatePullRequest {
        thread_id: ThreadId,
    },
    /// Stop the thread's agent and terminal and remove its worktree (the
    /// branch is kept). A dirty worktree needs `force`.
    ArchiveThread {
        thread_id: ThreadId,
        #[serde(default)]
        force: bool,
    },
    /// Delete a thread for good: stop it, remove its worktree (the branch is
    /// kept) and checkpoints, and erase its history.
    DeleteThread {
        thread_id: ThreadId,
    },
    /// Re-read `config.toml` (harness and worktree settings changed).
    ReloadConfig,
    /// Is the harness's CLI installed, where, and which version.
    ProbeHarness {
        harness: HarnessId,
    },
    /// Files under a root matching `query` fuzzily (composer `@` mentions).
    FindFiles {
        root: FsRoot,
        query: String,
        #[serde(default = "default_find_limit")]
        limit: u32,
    },
    /// The models a harness offers (cached by the daemon).
    ListModels {
        harness: HarnessId,
    },
    /// The slash commands (and skills) `harness` offers in `cwd`, asked of
    /// the agent without a session (answers `Response::Commands`).
    ListCommands {
        harness: HarnessId,
        cwd: String,
    },
    /// Ask the daemon to exit (graceful shutdown or upgrade).
    Shutdown {
        reason: ShutdownReason,
    },
    // -- workbench: files, diffs, terminals, browser, thread queries --------
    ListDir {
        root: FsRoot,
        #[serde(default)]
        rel_path: String,
    },
    ReadFile {
        root: FsRoot,
        rel_path: String,
    },
    GetDiff {
        thread_id: ThreadId,
        scope: DiffScope,
    },
    /// Open a shell. With `key`, a live terminal opened earlier with the same
    /// key is returned (resized, unless `cols`/`rows` are 0) instead, so a UI
    /// restart reattaches.
    TerminalOpen {
        root: FsRoot,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    TerminalInput {
        id: TerminalId,
        data: Bytes,
    },
    TerminalResize {
        id: TerminalId,
        cols: u16,
        rows: u16,
    },
    TerminalClose {
        id: TerminalId,
    },
    /// Live terminals whose key starts with `key_prefix` (a UI rebuilding a
    /// thread's terminal tabs after a restart).
    ListTerminals {
        key_prefix: String,
    },
    /// Open the thread's tab in the daemon's browser, or return the one it
    /// has (ADR-0017). `url` only applies to a new tab. Answers
    /// `Response::Page`; subscribe to [`Topic::Page`] for its pictures.
    PageOpen {
        thread_id: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        width: u32,
        height: u32,
        scale: f32,
    },
    PageNavigate {
        id: PageId,
        nav: PageNav,
    },
    PageInput {
        id: PageId,
        input: PageInput,
    },
    /// The viewer's size changed: the page lays out to it.
    PageResize {
        id: PageId,
        width: u32,
        height: u32,
        scale: f32,
    },
    /// The page's selected text (`Response::Text`), for the viewer's
    /// clipboard.
    PageSelection {
        id: PageId,
    },
    PageClose {
        id: PageId,
    },
    /// Run a browser command in the user's browser panel (from an agent).
    BrowserCall {
        thread_id: ThreadId,
        command: serde_json::Value,
    },
    /// An agent starts another thread (orchestration): a child of `from`,
    /// with `from`'s agent, model, effort and permissions unless given, in
    /// `from`'s workspace (or a new worktree from it), sent `prompt` at
    /// once. Answers `Response::Thread`.
    SpawnThread {
        from: ThreadId,
        prompt: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        harness: Option<HarnessId>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        worktree: bool,
        /// A thread of its own in the sidebar instead of one under `from`.
        #[serde(default)]
        top_level: bool,
        /// Start it in another project (its name, root path or id) instead
        /// of `from`'s.
        #[serde(default)]
        project: Option<String>,
        /// Start it as a chat (no project).
        #[serde(default)]
        chat: bool,
    },
    /// The projects, for agents choosing where to start threads
    /// (`Response::Json`: id, name, path).
    ListProjects,
    /// Where a thread stands, for an orchestrating agent: status, settings,
    /// its latest reply and who spawned it (`Response::Json`).
    ThreadOutcome {
        thread_id: ThreadId,
    },
    /// Threads whose messages contain `query` (case-insensitive), most
    /// recent first, one hit per thread (answers `Response::SearchHits`).
    SearchThreads {
        query: String,
        #[serde(default = "default_search_limit")]
        limit: u32,
    },
    /// A file by its path on the daemon's machine (`~`, `file://` and paths
    /// relative to `thread_id`'s folder allowed): images in replies,
    /// attachments, project icons. For a browser session the path must
    /// resolve inside a project, a thread's folder, or Agentloft's own
    /// attachments and icons (ADR-0016). Answers `Response::File`.
    ReadPath {
        path: String,
        #[serde(default)]
        thread_id: Option<ThreadId>,
    },
    // -- what the desktop app reads from disk, for clients that can't -------
    /// The settings files a UI needs (config.toml, colour themes, sidebar
    /// state), unless they still match `known` (a `UiFiles::stamp`).
    /// Answers `Response::UiFiles`.
    UiFiles {
        #[serde(default)]
        known: Option<String>,
    },
    /// Save a piece of UI state shared by every window and browser
    /// (`name`: `sidebar`), as JSON.
    PutUiState {
        name: String,
        json: String,
    },
    /// Delete a colour theme the user added (`custom:<name>`).
    DeleteTheme {
        id: String,
    },
    /// A project's icon: the image chosen for it, else the one found in its
    /// folder (answers `Response::File`; `NotFound` when it has none).
    ProjectIcon {
        project_id: ProjectId,
    },
    // -- web access (ADR-0016); local clients only -------------------------
    /// Web access as it stands: listening or not, the pairing link, and the
    /// browsers signed in (answers `Response::WebStatus`, as do the next
    /// three).
    WebStatus,
    /// Replace the pairing token and sign every browser out.
    WebRotateToken,
    /// Sign one browser out.
    WebRevokeSession {
        id: String,
    },
    /// Sign every browser out (the pairing link keeps working).
    WebRevokeAllSessions,
    /// Agent-facing thread queries (Agentloft MCP server, ADR-0013).
    ListThreads {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        project_id: Option<ProjectId>,
    },
    ReadThread {
        thread_id: ThreadId,
        #[serde(default)]
        detail: ReadDetail,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    User,
    Upgrade,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Topic {
    /// Projects and thread summaries.
    Workspace,
    /// Full timeline of one thread.
    Thread(ThreadId),
    /// Output of one terminal (snapshot replays the scrollback buffer).
    Terminal(TerminalId),
    /// Pictures and state of one tab of the daemon's browser.
    Page(PageId),
}

// ---------------------------------------------------------------------------
// Daemon → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome(Welcome),
    Response {
        id: u64,
        result: Result<Response, ErrorBody>,
    },
    Snapshot {
        sub_id: u64,
        snapshot: Snapshot,
    },
    Event {
        sub_id: u64,
        event: EventEnvelope,
    },
    Delta {
        sub_id: u64,
        delta: Delta,
    },
    TerminalOutput {
        sub_id: u64,
        id: TerminalId,
        data: Bytes,
    },
    TerminalExited {
        sub_id: u64,
        id: TerminalId,
        code: Option<i32>,
    },
    /// A new picture of a page. Acknowledge it with
    /// [`ClientMsg::PageFrameAck`] to get the next.
    PageFrame {
        sub_id: u64,
        id: PageId,
        frame: PageFrame,
    },
    /// A page's address, title, loading state or size changed.
    PageState {
        sub_id: u64,
        page: PageInfo,
    },
    /// The page is gone (closed, or the browser stopped).
    PageClosed {
        sub_id: u64,
        id: PageId,
        reason: String,
    },
    /// Daemon → client call (e.g. drive the browser panel); answer with
    /// [`ClientMsg::ClientCallResult`].
    ClientCall {
        call_id: u64,
        call: ClientCall,
    },
    /// Connection-level error (bad handshake, unsupported version…).
    Fatal(ErrorBody),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Welcome {
    pub protocol_version: u32,
    pub daemon_build: String,
    pub daemon_pid: u32,
    /// Modification time of the daemon's executable when it started. If the
    /// binary on disk changed since (rebuild, app update), the app restarts
    /// the daemon once it's idle.
    #[serde(default)]
    pub daemon_exe_mtime_ms: Option<i64>,
    /// Set for a browser (or other remote) session: what it may do beyond
    /// the defaults (ADR-0016). Absent for local clients, which may do
    /// everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteAccess>,
}

/// The limits of a remote session, so its UI can hide what it can't use.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAccess {
    /// Terminals (a shell on the daemon's machine) are allowed.
    #[serde(default)]
    pub terminal: bool,
    /// The daemon's browser (pages it loads on its machine) is allowed.
    #[serde(default)]
    pub browser: bool,
}

fn default_search_limit() -> u32 {
    100
}

fn default_find_limit() -> u32 {
    20
}

/// Modification time of a file in ms since the epoch (exe staleness check).
pub fn file_mtime_ms(path: &std::path::Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Ok,
    /// The agent's own report, as it gave it (shape depends on the kind).
    AgentInfo {
        info: serde_json::Value,
    },
    Project {
        project: Project,
    },
    Thread {
        thread: Thread,
    },
    DirListing {
        entries: Vec<DirEntry>,
    },
    File {
        file: FileContent,
    },
    Diff {
        diff: DiffResult,
    },
    Terminal {
        terminal: TerminalInfo,
    },
    Terminals {
        terminals: Vec<TerminalInfo>,
    },
    Page {
        page: PageInfo,
    },
    Branches {
        branches: BranchList,
    },
    Json {
        value: serde_json::Value,
    },
    Text {
        text: String,
    },
    Models {
        models: Vec<ModelInfo>,
    },
    Commands {
        commands: Vec<SlashCommandInfo>,
    },
    SearchHits {
        hits: Vec<SearchHit>,
    },
    HarnessStatus {
        status: HarnessStatus,
    },
    Paths {
        paths: Vec<String>,
    },
    WebStatus {
        status: WebStatus,
    },
    /// `None`: nothing changed since `known`.
    UiFiles {
        #[serde(default)]
        files: Option<Box<UiFiles>>,
    },
    /// A new fork, plus the handoff digest to offer as its first message.
    Forked {
        thread: Thread,
        #[serde(default)]
        draft: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
    Unsupported,
    Harness,
    Internal,
    IncompatibleVersion,
    /// This connection isn't allowed to do that (a browser session asking
    /// for something only the desktop app may do).
    Forbidden,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // wire type, built once per subscription
pub enum Snapshot {
    Workspace {
        seq: u64,
        projects: Vec<Project>,
        threads: Vec<Thread>,
    },
    Thread {
        seq: u64,
        thread: Thread,
        turns: Vec<Turn>,
        items: Vec<Item>,
        requests: Vec<RuntimeRequest>,
    },
    Terminal {
        seq: u64,
        terminal: TerminalInfo,
        replay: Bytes,
    },
    /// A page as it is now, with its latest picture if it has one (`seq`
    /// is that picture's).
    Page {
        seq: u64,
        page: PageInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<PageFrame>,
    },
}

/// A committed, persisted domain event with its global sequence number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub seq: u64,
    pub at_ms: i64,
    pub event: Event,
}

/// Domain events. Persisted in the daemon's event log and fanned out to
/// subscribers. Each variant carries the full new state of the entity it
/// touches, so clients can apply them without extra queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ProjectAdded {
        project: Project,
    },
    /// Renamed (or otherwise changed).
    ProjectUpdated {
        project: Project,
    },
    /// The project's entry is gone (its threads were deleted first; files
    /// on disk are untouched).
    ProjectRemoved {
        project_id: ProjectId,
    },
    ThreadCreated {
        thread: Thread,
    },
    ThreadUpdated {
        thread: Thread,
    },
    TurnStarted {
        turn: Turn,
    },
    TurnUpdated {
        turn: Turn,
    },
    ItemUpserted {
        item: Item,
    },
    RequestOpened {
        request: RuntimeRequest,
    },
    RequestUpdated {
        request: RuntimeRequest,
    },
    /// The thread and its history are gone.
    ThreadDeleted {
        thread_id: ThreadId,
    },
}

impl Event {
    /// The thread this event belongs to, if any (workspace-level otherwise).
    pub fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            Event::ProjectAdded { .. }
            | Event::ProjectUpdated { .. }
            | Event::ProjectRemoved { .. } => None,
            Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => Some(&thread.id),
            Event::TurnStarted { turn } | Event::TurnUpdated { turn } => Some(&turn.thread_id),
            Event::ItemUpserted { item } => Some(&item.thread_id),
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                Some(&request.thread_id)
            }
            Event::ThreadDeleted { thread_id } => Some(thread_id),
        }
    }

    /// Whether workspace subscribers should receive this event.
    pub fn is_workspace_event(&self) -> bool {
        matches!(
            self,
            Event::ProjectAdded { .. }
                | Event::ProjectUpdated { .. }
                | Event::ProjectRemoved { .. }
                | Event::ThreadCreated { .. }
                | Event::ThreadUpdated { .. }
                | Event::ThreadDeleted { .. }
        )
    }
}

/// Ephemeral streaming chunk. Not persisted; the item snapshot catches up.
///
/// `offset` is the length (in bytes) of the item's streamed text *before* this
/// chunk, so clients can drop chunks they already have from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delta {
    pub thread_id: ThreadId,
    pub item_id: ItemId,
    pub channel: DeltaChannel,
    pub offset: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeltaChannel {
    Text,
    Reasoning,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_round_trip() {
        let msgs = vec![
            ClientMsg::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                client_kind: ClientKind::LocalUi,
                client_build: "test".into(),
                capabilities: vec!["browser".into()],
            }),
            ClientMsg::Request {
                id: 7,
                body: Request::SendMessage {
                    thread_id: ThreadId::new(),
                    text: "hi".into(),
                    attachments: vec![],
                },
            },
            ClientMsg::Subscribe {
                sub_id: 1,
                topic: Topic::Thread(ThreadId::new()),
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back, "{json}");
        }
    }

    #[test]
    fn server_msg_round_trip() {
        let msg = ServerMsg::Response {
            id: 1,
            result: Err(ErrorBody::new(ErrorCode::NotFound, "no such thread")),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
    }
}

/// The settings files behind the UI, as text (`Request::UiFiles`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UiFiles {
    /// Changes when any of the files does.
    pub stamp: String,
    /// Agentloft's data home on the daemon's machine (`~/.agentloft`).
    pub home: String,
    /// The user's home folder there.
    pub user_home: String,
    /// `config.toml` (empty if there is none). A browser session gets it
    /// with agents' environment values hidden and `[web]` left out.
    pub config_toml: String,
    /// Colour themes in `<home>/themes`: file stem and TOML.
    #[serde(default)]
    pub themes: Vec<UiFile>,
    /// `ui-sidebar.json`: folded and pinned projects, folded threads.
    #[serde(default)]
    pub sidebar: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UiFile {
    pub name: String,
    pub text: String,
}

/// Web access as the daemon sees it (Settings ▸ Web access).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebStatus {
    /// `[web] enabled` in config.toml.
    pub enabled: bool,
    /// The listener is up.
    pub running: bool,
    /// Why it isn't, when it should be (bad address, port in use, a
    /// non-loopback address without TLS, unreadable certificate…).
    #[serde(default)]
    pub error: Option<String>,
    /// Where it listens: `http://127.0.0.1:4780`.
    #[serde(default)]
    pub url: Option<String>,
    /// `url` with the pairing token in its fragment. Secret: opening it
    /// signs a browser in.
    #[serde(default)]
    pub pairing_url: Option<String>,
    #[serde(default)]
    pub tls: bool,
    /// Reachable from other machines.
    #[serde(default)]
    pub remote: bool,
    /// Reachable from other machines over plain HTTP.
    #[serde(default)]
    pub insecure_remote: bool,
    /// The web client's files were found (`cargo xtask web`, or the app
    /// bundle's copy).
    #[serde(default)]
    pub bundle: bool,
    #[serde(default)]
    pub sessions: Vec<WebSession>,
}

/// A browser signed in to web access.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebSession {
    /// Names the session for revoking it; not a secret.
    pub id: String,
    /// "Chrome on macOS", from the browser's User-Agent.
    pub label: String,
    /// The address it signed in from.
    pub addr: String,
    pub created_ms: i64,
    pub last_seen_ms: i64,
    pub expires_ms: i64,
    /// Connections open right now.
    #[serde(default)]
    pub connections: u32,
}

/// A report an agent can give about itself.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentInfoKind {
    /// What fills the context window.
    Context,
    /// Plan limits, session cost and tips.
    Usage,
    /// Account, limits and this thread's tokens (Codex's `/status`).
    Status,
}
