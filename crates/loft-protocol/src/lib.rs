//! Wire types shared by `agentloftd` and its clients (PLAN §1.6).
//!
//! Everything here is plain serde data: no I/O, no async, no GPUI. Frames are
//! length-delimited JSON (`u32` big-endian length prefix) carrying [`ClientMsg`]
//! and [`ServerMsg`].

pub mod automation;
pub mod devices;
pub mod ids;
pub mod model;
pub mod pages;
pub mod workbench;

pub use automation::*;
pub use devices::*;
pub use ids::*;
pub use model::*;
pub use pages::*;
pub use workbench::*;

use serde::{Deserialize, Serialize};

/// For `skip_serializing_if`: an offset of 0 is left off the wire.
pub(crate) fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// For `skip_serializing_if`: `false` is left off the wire.
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// Bumped on any incompatible wire change. The daemon accepts `N` and `N-1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest message either side sends or accepts (256 MiB). A long thread's
/// snapshot alone runs to tens of megabytes; nothing should come near this.
pub const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

/// Largest message the daemon accepts from a web session (16 MiB): requests
/// are small, and the browser is on the network.
pub const MAX_REMOTE_FRAME_LEN: usize = 16 * 1024 * 1024;

/// The capability of a browser that unpacks compressed messages: the daemon
/// then sends any message of [`DEFLATE_FROM`] bytes or more as a binary
/// WebSocket message holding raw deflate (RFC 1951) of its JSON. Smaller
/// ones stay text. A thread's snapshot can be megabytes, and over a slow
/// link shrinking it fourfold saves seconds per thread switch.
pub const DEFLATE: &str = "deflate";
/// The capability of an app on the daemon's own computer: someone at it
/// hears what it plays. The pages of the daemon's browser it watches play
/// their sound on that computer's speakers, unless someone elsewhere (a
/// phone) used the page since. Every other tab is muted. They are also
/// pictured for it at every device pixel and as often as Chrome draws:
/// nothing between it and the browser is slower than memory. Only counts on
/// the daemon's socket, never from a browser session.
pub const SOUND: &str = "sound";
/// Messages at least this long are compressed for a [`DEFLATE`] client.
pub const DEFLATE_FROM: usize = 8 * 1024;

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
    /// This viewer has shown [`ServerMsg::PageFrame`] (or
    /// [`ServerMsg::DeviceFrame`]) `seq` and is ready for the next. One frame is in flight per subscription, so a slow link
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
    /// Optional client capabilities, e.g. [`DEFLATE`] (a browser that unpacks
    /// compressed messages) or [`SOUND`]. Unknown ones are ignored
    /// (`"browser"`, which app windows sent while they hosted agents'
    /// browser, is now unused).
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
    /// Tokens and cost over every thread's turns (`Response::Json`): totals,
    /// and per day, agent, model, project and thread over the days
    /// `from_day..=to_day` (unset: since the first turn / up to today).
    /// Days are local to the asker (`utc_offset_min` east of UTC), counted
    /// from 1970-01-01.
    UsageReport {
        #[serde(default)]
        utc_offset_min: i32,
        #[serde(default)]
        from_day: Option<i64>,
        #[serde(default)]
        to_day: Option<i64>,
    },
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
    /// Hold a message in the thread's queue: it is sent when the running
    /// turn completes (after those ahead of it). With no turn running and
    /// nothing ahead, it is sent at once. Answers `Response::Ok`.
    Enqueue {
        thread_id: ThreadId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Take a queued message out: dropped, or back to the composer.
    Dequeue {
        thread_id: ThreadId,
        id: u64,
    },
    /// Move queued message `id` to where `to` is: after it when moving
    /// down, before it when moving up. The first in line goes next.
    MoveQueued {
        thread_id: ThreadId,
        id: u64,
        to: u64,
    },
    /// Send a queued message now: into the running turn (the agent takes it
    /// in at its next step), or as the next turn when none is running. The
    /// rest stay queued.
    SendQueued {
        thread_id: ThreadId,
        id: u64,
    },
    /// For a thread its agent's usage limit stopped (`Thread::limit`):
    /// carry on by itself when the limit resets (`on`), or don't.
    ResumeAtReset {
        thread_id: ThreadId,
        on: bool,
    },
    /// Stops the running turn, and the turns of threads this one's agent
    /// started (and theirs).
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
    /// harness gets a handoff digest saved as inherited context.
    /// The fork gets its own worktree from the parent's
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
    /// Send what was asked in a turn (its messages and their images) again,
    /// as a new turn: for one that failed (signed out, network down, …).
    RetryTurn {
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
    /// Ask GitHub about the thread's pull request now (its state, checks and
    /// review), or look for one on its branch when none is known. What is
    /// new arrives as a thread update; answers `Response::Ok` either way.
    RefreshPullRequest {
        thread_id: ThreadId,
    },
    /// Stop the thread's agent and terminal and remove its worktree (the
    /// branch is kept). A dirty worktree needs `force`.
    ArchiveThread {
        thread_id: ThreadId,
        #[serde(default)]
        force: bool,
    },
    /// Delete a thread for good: stop it, remove its worktree and
    /// checkpoints, and erase its history. Its branch is kept unless its
    /// work is in the base branch and `worktrees.delete_branch` is on.
    DeleteThread {
        thread_id: ThreadId,
    },
    /// Remove a worktree thread's worktree (and, per `[worktrees]`, its
    /// branch) once everything on it is in the base branch. The thread
    /// stays (`ThreadWorkspace::Released`); its next message gets a new
    /// worktree. Refused while anything is running there or its work isn't
    /// in the base yet. Answers `Response::Thread`.
    CleanUpWorktree {
        thread_id: ThreadId,
    },
    /// Bring back the branch a cleanup deleted, where it was. Answers
    /// `Response::Thread`.
    RestoreBranch {
        thread_id: ThreadId,
    },
    /// Worktrees and Agentloft branches that no active thread works in,
    /// across projects, and whether their work is in the main branch.
    /// Answers `Response::WorktreeLeftovers`.
    WorktreeLeftovers,
    /// Remove these leftovers. Each is checked again first: only clean
    /// folders, and only branches whose work is in the main branch, go.
    /// Answers `Response::Text` with a summary.
    RemoveWorktreeLeftovers {
        leftovers: Vec<WorktreeLeftover>,
    },
    /// Re-read `config.toml` (harness and worktree settings changed).
    ReloadConfig,
    /// Is the harness's CLI installed, where, and which version.
    ProbeHarness {
        harness: HarnessId,
    },
    /// Install the newer release `ProbeHarness` found (its
    /// `HarnessUpdate::command`). Answers `Response::HarnessStatus` once
    /// done; an error carries the end of the command's output.
    UpdateHarness {
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
    /// Answers `Response::File` with the file from `offset` on, as much as
    /// one answer holds; `FileContent::truncated` says there is more.
    ReadFile {
        root: FsRoot,
        rel_path: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: u64,
    },
    GetDiff {
        thread_id: ThreadId,
        scope: DiffScope,
    },
    /// More of a thread than its snapshot carries (`Snapshot::Thread::older`).
    ThreadItems {
        thread_id: ThreadId,
        page: ItemsPage,
    },
    /// Open a shell. With `key`, a live terminal opened earlier with the same
    /// key is returned (resized, unless `cols`/`rows` are 0) instead, so a UI
    /// restart reattaches. `command` is typed into the shell as its user
    /// would (so it can be stopped and run again there): into a new one
    /// once it is ready, into a live one only if nothing is running in it.
    TerminalOpen {
        root: FsRoot,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
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
    /// Open one of the thread's tabs in the daemon's browser (ADR-0017):
    /// `tab` if it is still open, a new one with `new_tab`, else the tab the
    /// thread used last (a new one if it has none). `url` only applies to a
    /// new tab. The tab takes the size asked for; a zero width or height
    /// leaves it as it is (a viewer showing it small). Answers
    /// `Response::Page`; subscribe to [`Topic::Page`] for its pictures.
    PageOpen {
        thread_id: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        width: u32,
        height: u32,
        scale: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<PageId>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        new_tab: bool,
    },
    /// The thread's open tabs, oldest first (`Response::Pages`).
    PageList {
        thread_id: ThreadId,
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
    /// The page's zoom (1 is 100%; kept between 0.25 and 5).
    PageZoom {
        id: PageId,
        zoom: f32,
    },
    /// Show the page as `device`, or at its viewers' size (`None`).
    PageEmulate {
        id: PageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<PageDevice>,
    },
    /// Forget what sites stored in the daemon's browser: cookies and site
    /// data, the cache, or both. Every page of that browser is affected.
    PageClearData {
        id: PageId,
        #[serde(default)]
        cookies: bool,
        #[serde(default)]
        cache: bool,
    },
    /// A PNG of the page as it shows (notes' pins included), at its scale:
    /// `Response::Picture`.
    PageCapture {
        id: PageId,
    },
    /// Turn annotation mode on or off. Notes come back as
    /// [`PageAction::Note`] to the viewer who left them.
    PageAnnotate {
        id: PageId,
        on: bool,
    },
    /// Take note `n`'s pin off the page, or every pin (`None`).
    PageUnpin {
        id: PageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<u32>,
    },
    /// A person pressed **Continue** (or **Done**) on the card of a tab
    /// its agent handed them ([`PageHandoff`]): the agent goes on.
    PageHandoffDone {
        id: PageId,
    },
    /// Open the page that draws a view an agent made (the `show_ui` call
    /// `item_id`, ADR-0018), or return the one it has. Answers
    /// `Response::Page`; subscribe to [`Topic::Page`] for its pictures and
    /// send [`Request::PageInput`] as for any page. The page is as wide as
    /// asked and as tall as its content; it closes by itself a while after
    /// its last viewer leaves.
    ViewOpen {
        thread_id: ThreadId,
        item_id: ItemId,
        width: u32,
        scale: f32,
        theme: Box<ViewTheme>,
    },
    /// The viewer's look changed (light to dark, another accent).
    ViewTheme {
        id: PageId,
        theme: Box<ViewTheme>,
    },
    /// What a view asked to be remembered (`agentloft.setState`), for a
    /// client that shows the view itself rather than the daemon's page.
    /// Answers `Response::Json` (`null` if nothing was kept).
    ViewState {
        thread_id: ThreadId,
        item_id: ItemId,
    },
    /// Remember this for a view (from a client that shows it itself).
    ViewKeep {
        thread_id: ThreadId,
        item_id: ItemId,
        state: serde_json::Value,
    },
    /// An agent's `show_ui` call, from the MCP bridge: check the page, make
    /// sure the thread has an item for it (a harness that didn't report the
    /// call gets one made), and load it once to see whether its script
    /// runs. Answers `Response::Json`: `{ item_id, problems: [..] }`.
    ShowView {
        thread_id: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        html: String,
    },
    /// Run a browser command (a `loft_page_script::BrowserCommand` as JSON)
    /// on one of the thread's tabs of the daemon's browser, opening one if
    /// it has none (from an agent). Answers `Response::Browser`.
    BrowserCall {
        thread_id: ThreadId,
        command: serde_json::Value,
        /// Which tab: its id, or the name the agent gave it. None: the tab
        /// the thread used last (ADR-0017 §9).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<String>,
    },
    /// The simulators and emulators on the daemon's computer, and what it
    /// can run (ADR-0020). Answers `Response::Devices`.
    DeviceList,
    /// Boot a stopped device, headless (its screen is only streamed).
    /// Answers `Ok` at once; its state comes on [`Topic::Devices`] and
    /// [`Topic::Device`]. `thread_id`: the thread whose agent asked, if one.
    /// `wait` (agents): answer only once it runs, with
    /// `Response::Device`, or fail if it doesn't within a few minutes.
    DeviceBoot {
        id: DeviceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<ThreadId>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        wait: bool,
    },
    DeviceShutdown {
        id: DeviceId,
    },
    /// Touches, keys, buttons. Answers `Ok` once the device took them.
    DeviceInput {
        id: DeviceId,
        input: DeviceInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<ThreadId>,
    },
    /// A PNG of the screen at its full size (`Response::Picture`).
    DeviceCapture {
        id: DeviceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<ThreadId>,
    },
    /// Install, start or stop an app, or open an address (`Ok`).
    DeviceApp {
        id: DeviceId,
        app: DeviceApp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<ThreadId>,
    },
    DeviceSet {
        id: DeviceId,
        setting: DeviceSetting,
    },
    /// The device's settings as they are now (`Response::DeviceSettings`).
    DeviceSettings {
        id: DeviceId,
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
        /// Where in the file to start, as for `ReadFile`.
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: u64,
    },
    /// Which of `paths` are files, resolved as for `ReadPath` (and as
    /// confined for a browser session, where "outside" reads as "no"): for
    /// clients that can't look at the daemon's disk, so a path in a reply
    /// can link to its file. At most [`MAX_FILES_EXIST`] paths. Answers
    /// `Response::FilesExist`.
    FilesExist {
        paths: Vec<String>,
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
    /// The commands a project runs often (`[[actions]]` in its
    /// `.agentloft/config.toml`), read from the file as it is now. Answers
    /// `Response::Actions`: none when it has no such file.
    ProjectActions {
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
    /// Start linking this computer to the website (ADR-0019): the answer's
    /// `website.pending` holds the page to open and the code it shows.
    WebLinkStart,
    /// Stop waiting for a link started with `WebLinkStart`.
    WebLinkCancel,
    /// Unlink this computer from the website.
    WebUnlink,
    /// Check the tunnel and the website again now, rather than within the
    /// minute (after fixing what the status said was wrong).
    WebSync,
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
    // -- automations -------------------------------------------------------
    /// Every automation (`Response::Json`: a list of [`Automation`]s).
    ListAutomations,
    /// Create an automation (`id: None`) or replace one's spec. `from`: the
    /// thread whose agent asks. Answers `Response::Automation`.
    SaveAutomation {
        #[serde(default)]
        id: Option<AutomationId>,
        spec: AutomationSpec,
        #[serde(default)]
        from: Option<ThreadId>,
    },
    /// Pause or resume one.
    SetAutomationEnabled {
        id: AutomationId,
        enabled: bool,
    },
    DeleteAutomation {
        id: AutomationId,
    },
    /// Run one now, whatever its schedule (`Response::Thread`: the run's
    /// thread).
    RunAutomation {
        id: AutomationId,
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
    /// The list of simulators and emulators, as they come, go, boot and
    /// stop (ADR-0020).
    Devices,
    /// Pictures and state of one simulator or emulator. Watching it is
    /// what makes it stream; it doesn't boot it.
    Device(DeviceId),
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
    /// A new picture of a device's screen. Acknowledge it with
    /// [`ClientMsg::PageFrameAck`] to get the next.
    DeviceFrame {
        sub_id: u64,
        id: DeviceId,
        frame: DeviceFrame,
    },
    /// A device's state, size or error changed ([`Topic::Device`]).
    DeviceState {
        sub_id: u64,
        device: DeviceInfo,
    },
    /// The devices now ([`Topic::Devices`]).
    DeviceList {
        sub_id: u64,
        devices: Vec<DeviceInfo>,
        support: DeviceSupport,
    },
    /// An agent is using a device for `thread_id`: sent to subscribers of
    /// that thread (`Topic::Thread`), at most about once a second per
    /// device, so a window can show it while the agent works.
    DeviceDriven {
        thread_id: ThreadId,
        device: DeviceInfo,
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
    /// A thread's browser tabs changed (one opened or closed): all of them,
    /// oldest first. Sent to subscribers of the thread, so every window
    /// showing it has the same tabs.
    PageTabs {
        thread_id: ThreadId,
        pages: Vec<PageInfo>,
    },
    /// The page is gone (closed, or the browser stopped).
    PageClosed {
        sub_id: u64,
        id: PageId,
        reason: String,
    },
    /// A view asked for something on this viewer's click.
    PageAction {
        sub_id: u64,
        id: PageId,
        action: PageAction,
    },
    /// An agent is using `thread_id`'s page of the daemon's browser (its
    /// `browser_*` tools, ADR-0017): sent to each connection subscribed to
    /// that thread (`Topic::Thread`), at most about once a second per page,
    /// so a window can show the page while the agent works. Not tied to a
    /// subscription; a browser session that may not use the browser isn't
    /// sent it.
    PageDriven {
        thread_id: ThreadId,
        page: PageInfo,
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
    /// For `FilesExist`: one answer per path, in order.
    FilesExist {
        files: Vec<bool>,
    },
    Diff {
        diff: DiffResult,
    },
    /// A page of a thread's items, oldest first. `older`: there are earlier
    /// turns still (for `ItemsPage::Before`).
    Items {
        items: Vec<Item>,
        #[serde(default, skip_serializing_if = "is_false")]
        older: bool,
    },
    Terminal {
        terminal: TerminalInfo,
    },
    Terminals {
        terminals: Vec<TerminalInfo>,
    },
    Actions {
        actions: Vec<ProjectAction>,
    },
    Page {
        page: PageInfo,
    },
    Branches {
        branches: BranchList,
    },
    WorktreeLeftovers {
        leftovers: Vec<WorktreeLeftover>,
    },
    Json {
        value: serde_json::Value,
    },
    /// A thread's browser tabs, oldest first.
    Pages {
        pages: Vec<PageInfo>,
    },
    /// What an agent's browser command answered (`Request::BrowserCall`).
    Browser {
        reply: Box<BrowserReply>,
    },
    /// `Request::DeviceSettings`.
    DeviceSettings {
        settings: DeviceSettings,
    },
    /// `Request::DeviceList`.
    Devices {
        devices: Vec<DeviceInfo>,
        support: DeviceSupport,
    },
    /// One device (`Request::DeviceBoot` with `wait`).
    Device {
        device: DeviceInfo,
    },
    /// A picture (`Request::PageCapture`, `Request::DeviceCapture`).
    Picture {
        png: Bytes,
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
    /// A new fork with its inherited context already saved.
    Forked {
        thread: Thread,
        /// Legacy composer prefill; new forks leave this empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<String>,
    },
    Automation {
        automation: Automation,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        automations: Vec<Automation>,
    },
    Thread {
        seq: u64,
        thread: Thread,
        turns: Vec<Turn>,
        items: Vec<Item>,
        requests: Vec<RuntimeRequest>,
        /// Messages waiting for the running turn to end, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        queue: Vec<QueuedMessage>,
        /// A long thread comes in part: `items` holds its last turns, and of
        /// each subagent only its latest steps. Earlier turns and a
        /// subagent's whole run come with `Request::ThreadItems`. `true` when
        /// earlier turns were left out.
        #[serde(default, skip_serializing_if = "is_false")]
        older: bool,
        /// The earlier turns left out: each one's message and the start of
        /// its reply (the turn markers beside the timeline, recall).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outline: Vec<TurnOutline>,
    },
    Terminal {
        seq: u64,
        terminal: TerminalInfo,
        replay: Bytes,
    },
    Devices {
        devices: Vec<DeviceInfo>,
        support: DeviceSupport,
    },
    /// A device as it is now, with its latest picture if it has one.
    Device {
        seq: u64,
        device: DeviceInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<DeviceFrame>,
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
    /// A thread's queue as it is now (empty: nothing waits).
    QueueUpdated {
        thread_id: ThreadId,
        queue: Vec<QueuedMessage>,
    },
    /// Created, changed, or run (its state carries the run's outcome).
    AutomationUpdated {
        automation: Automation,
    },
    AutomationDeleted {
        automation_id: AutomationId,
    },
}

impl Event {
    /// The thread this event belongs to, if any (workspace-level otherwise).
    pub fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            Event::ProjectAdded { .. }
            | Event::ProjectUpdated { .. }
            | Event::ProjectRemoved { .. }
            | Event::AutomationUpdated { .. }
            | Event::AutomationDeleted { .. } => None,
            Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => Some(&thread.id),
            Event::TurnStarted { turn } | Event::TurnUpdated { turn } => Some(&turn.thread_id),
            Event::ItemUpserted { item } => Some(&item.thread_id),
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                Some(&request.thread_id)
            }
            Event::ThreadDeleted { thread_id } | Event::QueueUpdated { thread_id, .. } => {
                Some(thread_id)
            }
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
                | Event::AutomationUpdated { .. }
                | Event::AutomationDeleted { .. }
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
    fn a_thread_without_commands_is_the_thread_but_for_them() {
        // Every field set to something that isn't its default, from JSON so
        // that a field added later can be added here without a constructor.
        let json = serde_json::json!({
            "id": "t1", "project_id": "p1", "title": "A thread", "harness": "claude",
            "cwd": "/repo/wt", "model": "fable", "permission_mode": "accept-edits", "effort": "high",
            "native_session_id": "s1", "status": "running",
            "created_at_ms": 5, "updated_at_ms": 9,
            "workspace": {"kind": "worktree", "path": "/repo/wt", "branch": "b", "base": "main"},
            "parent": {"thread_id": "t0", "turn_id": "u0", "kind": "spawned", "detached": true},
            "slash_commands": ["help", "compact"],
            "command_info": [{"name": "help", "description": "What you can do"}],
            "started_at_ms": 6, "draft": "half a thought", "waiting_on": ["a dev server"],
            "suggestion": "run the tests",
        });
        let thread: Thread = serde_json::from_value(json).unwrap();
        assert!(!thread.command_info.is_empty() && !thread.slash_commands.is_empty());
        let bare = thread.without_commands();
        assert!(bare.command_info.is_empty() && bare.slash_commands.is_empty());
        let mut expected = thread.clone();
        expected.slash_commands.clear();
        expected.command_info.clear();
        assert_eq!(bare, expected);
        // Nothing above was left at a default that would hide a field the
        // copy dropped.
        let as_json = serde_json::to_value(&bare).unwrap();
        for field in [
            "model",
            "permission_mode",
            "effort",
            "native_session_id",
            "parent",
            "started_at_ms",
            "draft",
            "waiting_on",
            "suggestion",
        ] {
            assert!(!as_json[field].is_null(), "{field}");
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

        // An agent is using a thread's page.
        let thread_id = ThreadId::new();
        let driven = ServerMsg::PageDriven {
            thread_id: thread_id.clone(),
            page: PageInfo {
                id: PageId::new(),
                thread_id,
                url: "https://example.com/".into(),
                title: "Example".into(),
                loading: false,
                can_go_back: false,
                can_go_forward: false,
                width: 1280,
                height: 800,
                scale: 2.0,
                view: false,
                overflow: false,
                cursor: String::new(),
                zoom: 1.0,
                device: None,
                annotating: false,
                editing: false,
                fields: Vec::new(),
                agent: Default::default(),
            },
        };
        let value = serde_json::to_value(&driven).unwrap();
        assert_eq!(value["type"], "page_driven");
        assert_eq!(value["page"]["width"], 1280);
        // A person's tab says nothing of agents.
        assert!(value["page"].get("agent").is_none());
        assert_eq!(serde_json::from_value::<ServerMsg>(value).unwrap(), driven);

        // An agent's tab, handed to a person.
        let mut handed = driven.clone();
        if let ServerMsg::PageDriven { page, .. } = &mut handed {
            page.agent = PageAgent {
                name: Some("docs".into()),
                opened: true,
                allowed_origins: vec!["example.com".into()],
                read_only: false,
                handoff: Some(PageHandoff {
                    kind: HandoffKind::Handoff,
                    reason: "Sign in".into(),
                    origin: "https://example.com".into(),
                    since_ms: 1,
                }),
            };
        }
        let value = serde_json::to_value(&handed).unwrap();
        assert_eq!(value["page"]["agent"]["handoff"]["kind"], "handoff");
        assert_eq!(serde_json::from_value::<ServerMsg>(value).unwrap(), handed);
    }

    #[test]
    fn browser_calls_name_their_tab_only_when_asked() {
        let call = Request::BrowserCall {
            thread_id: ThreadId("t".into()),
            command: serde_json::json!({ "action": "snapshot" }),
            tab: None,
        };
        let value = serde_json::to_value(&call).unwrap();
        assert!(value.get("tab").is_none());
        // What an older bridge sends still reads.
        let old = serde_json::json!({
            "op": "browser_call", "thread_id": "t", "command": { "action": "back" }
        });
        assert!(matches!(
            serde_json::from_value::<Request>(old).unwrap(),
            Request::BrowserCall { tab: None, .. }
        ));
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

/// The most paths one `Request::FilesExist` may ask about.
pub const MAX_FILES_EXIST: usize = 128;

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
    /// Network access is on: only private networks may connect.
    #[serde(default)]
    pub network: bool,
    /// Where other devices (and this one) can reach it, the one the
    /// pairing link uses marked `default`.
    #[serde(default)]
    pub addresses: Vec<WebAddress>,
    /// Public with Cloudflare Tunnel (`[web] tunnel`), when asked for.
    #[serde(default)]
    pub tunnel: Option<TunnelStatus>,
    /// The website this computer is linked to, or being linked to.
    #[serde(default)]
    pub website: WebsiteStatus,
}

/// Public with Cloudflare Tunnel: `cloudflared` carries this computer's own
/// address on the internet to the listener (ADR-0019).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TunnelStatus {
    /// `starting` (setting up, or connecting), `on`, `missing` (no
    /// cloudflared here), `error`, `waiting` (asked for, but held until
    /// something else is done: a quick tunnel until the computer is linked).
    pub state: String,
    /// `https://agentloft-desk.example.com`.
    #[serde(default)]
    pub url: Option<String>,
    /// What's wrong, in words.
    #[serde(default)]
    pub error: Option<String>,
}

/// This computer and the website it can be opened from (ADR-0019).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebsiteStatus {
    /// `https://agentloft.promptanddone.com`.
    #[serde(default)]
    pub url: String,
    /// Linked to this account (an email address).
    #[serde(default)]
    pub account: Option<String>,
    /// The address it last told the website; `None`: none to give.
    #[serde(default)]
    pub reported_url: Option<String>,
    /// Waiting for someone to approve the link in a browser.
    #[serde(default)]
    pub pending: Option<WebsiteLinkPending>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A link waiting to be approved on the website.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebsiteLinkPending {
    /// The page to open; it asks to sign in, then to approve.
    pub verify_url: String,
    /// Shown on both sides, to compare: `BCDF-GH23`.
    pub user_code: String,
    pub expires_ms: i64,
}

/// One address web access can be reached at.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WebAddress {
    /// `local` (this computer only), `lan` (the local network), `name` (a
    /// name in `allowed_hosts`) or `cloudflare` (the public name,
    /// Cloudflare Tunnel).
    pub kind: String,
    /// The host part (`192.168.1.209`), what `[web] link_host` names.
    pub host: String,
    /// `http://192.168.1.209:4780`.
    pub url: String,
    /// The pairing link uses this one.
    #[serde(default)]
    pub default: bool,
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
