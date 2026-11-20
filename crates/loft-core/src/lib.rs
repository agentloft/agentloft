//! The Agentloft core (PLAN §1.5): handles client requests, runs one actor per
//! active thread that drives its harness session, persists domain events and
//! fans them out on a broadcast bus together with ephemeral streaming deltas.

mod agent_limits;
mod agent_updates;
mod automations;
mod browser;
mod cleanup;
pub mod devices;
mod forks;
mod handoff;
mod item_pages;
mod limits;
mod pages;
mod pull_requests;
mod remote;
mod scripts;
mod settings;
mod suggestions;
mod terminals;
mod thread;
mod titles;
mod usage;
mod voice;
mod workbench;
mod worktrees;

pub use automations::{describe as describe_schedule, local_text, parse_local};

use anyhow::Result;
use loft_harness::Harness;
use loft_protocol::{
    Delta, ErrorBody, ErrorCode, Event, EventEnvelope, HarnessId, Item, ItemId, ItemStatus,
    Project, ProjectId, Request, RequestStatus, Response, Snapshot, Thread, ThreadId, ThreadStatus,
    Topic, TurnStatus,
};
use loft_store::{Store, now_ms};
pub use remote::{RemotePolicy, redact_settings};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use thread::{ThreadCmd, ThreadHandle};
use tokio::sync::{broadcast, oneshot};

/// A directory's file list and when it was taken.
type FileIndex = (std::time::Instant, Arc<Vec<String>>);

/// Message on the core bus: a committed event or a streaming delta.
#[derive(Debug, Clone)]
pub enum BusMsg {
    /// Shared: the bus clones each message per subscriber.
    Event(Arc<EventEnvelope>),
    Delta(Delta),
    /// A terminal's output stream now ends at byte `end`. Carries no data:
    /// subscribers read what's new from the terminal's ring buffer, so a
    /// burst costs no memory beyond the ring and coalesces naturally.
    TerminalOutput {
        id: loft_protocol::TerminalId,
        end: u64,
    },
    TerminalExited {
        id: loft_protocol::TerminalId,
        code: Option<i32>,
    },
    /// A page of the daemon's browser has a new picture, numbered `seq`.
    /// Carries no data: a viewer takes the newest when it is ready.
    PageFrame {
        id: loft_protocol::PageId,
        seq: u64,
    },
    /// A page's address, title, loading state or size changed.
    PageState {
        id: loft_protocol::PageId,
    },
    PageClosed {
        id: loft_protocol::PageId,
        reason: String,
    },
    /// A view asked for something on a click. `to`: the connection that
    /// clicked, when it is known (else every viewer hears it).
    PageAction {
        id: loft_protocol::PageId,
        action: loft_protocol::PageAction,
        to: Option<u64>,
    },
    /// An agent is using the thread's page (said at most once a second per
    /// page), for whoever has the thread open.
    PageDriven {
        thread_id: ThreadId,
        page: loft_protocol::PageInfo,
    },
    /// A device's screen has a new picture, numbered `seq` (ADR-0020).
    DeviceFrame {
        id: loft_protocol::DeviceId,
        seq: u64,
    },
    /// A device's state, size or error changed.
    DeviceState {
        id: loft_protocol::DeviceId,
    },
    /// The list of devices changed.
    DevicesChanged,
    /// An agent of `thread_id` is using a device (at most once a second).
    DeviceDriven {
        thread_id: ThreadId,
        device: loft_protocol::DeviceInfo,
    },
    /// The thread's tabs changed (one opened or closed): all of them.
    PageTabs {
        thread_id: ThreadId,
        pages: Vec<loft_protocol::PageInfo>,
    },
}

/// How wide a view is laid out when it is loaded with nobody looking yet
/// (to see whether its script runs).
const UNWATCHED_VIEW_WIDTH: u32 = 760;
/// How long a view gets to load and run before `show_ui` answers.
const VIEW_CHECK: Duration = Duration::from_millis(2500);

/// How agent sessions reach the Agentloft MCP server (`agentloftd mcp`).
#[derive(Debug, Clone)]
pub struct McpBridge {
    pub exe: PathBuf,
    pub home: PathBuf,
}

/// Names the thread an agent process works for, in its environment (the
/// `agent-device` command line passes it on to the daemon).
pub const THREAD_ENV: &str = "AGENTLOFT_THREAD_ID";

impl McpBridge {
    /// Where the commands agents run in their shells live (`agent-device`);
    /// first on their `PATH`.
    pub fn bin_dir(&self) -> PathBuf {
        self.home.join("bin")
    }

    /// Write `agent-device` into [`McpBridge::bin_dir`]: a script that runs
    /// `agentloftd device` on this home (ADR-0020). A shell script on Unix;
    /// on Windows a `.cmd` (PowerShell, cmd) and the shell script too (Git
    /// Bash, which runs Claude Code's commands there, doesn't run `.cmd`
    /// files by their bare name).
    pub fn install_cli(&self) -> std::io::Result<()> {
        let dir = self.bin_dir();
        std::fs::create_dir_all(&dir)?;
        let (exe, home) = (self.exe.to_string_lossy(), self.home.to_string_lossy());
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let sh = format!(
            "#!/bin/sh\n# Written by Agentloft at each start: agents' device command line.\n\
             exec {} --home {} device \"$@\"\n",
            quote(&exe),
            quote(&home)
        );
        write_executable(&dir.join("agent-device"), &sh)?;
        if cfg!(windows) {
            let cmd = format!(
                "@echo off\r\nrem Written by Agentloft at each start: agents' device command line.\r\n\
                 \"{}\" --home \"{}\" device %*\r\n",
                exe.replace('%', "%%"),
                home.replace('%', "%%")
            );
            write_executable(&dir.join("agent-device.cmd"), &cmd)?;
        }
        Ok(())
    }
}

/// Write a file whole (a script being run never sees half of it), runnable.
fn write_executable(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, path)
}

/// Slash commands per (harness, folder), with when they were asked.
type CommandCache =
    HashMap<(String, String), (std::time::Instant, Vec<loft_protocol::SlashCommandInfo>)>;

pub struct CoreOptions {
    pub env: Arc<HashMap<String, String>>,
    pub config: loft_config::Config,
    /// Snapshot cadence for streaming items (ADR-0006: ≈250 ms).
    pub flush_interval: Duration,
    /// Attach the Agentloft MCP server to agent sessions.
    pub mcp_bridge: Option<McpBridge>,
    /// Where per-thread worktrees go (`~/.agentloft/worktrees`).
    /// Where chats (threads without a project) work.
    pub chats_dir: PathBuf,
    /// Where new projects (made from a name) go.
    pub projects_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    /// `config.toml`, re-read on `Request::ReloadConfig`.
    pub config_file: Option<PathBuf>,
    /// Where uploaded attachments are stored (`~/.agentloft/attachments`).
    pub attachments_dir: PathBuf,
    /// Asks the web for the agents' latest releases (`None`: never asks).
    pub fetch: Option<Fetcher>,
    /// Simulators and emulators (ADR-0020): `devices::system_backends` in
    /// the daemon, none (or fakes) in tests.
    pub device_backends: Vec<Arc<dyn devices::Backend>>,
    /// Where downloaded models go (`~/.agentloft/models`).
    pub models_dir: PathBuf,
    /// The program that runs voice models and its arguments
    /// (`agentloftd voice-engine`); `None`: no voice input (tests).
    pub voice_engine: Option<(PathBuf, Vec<String>)>,
}

pub use agent_updates::Fetcher;

impl Default for CoreOptions {
    fn default() -> Self {
        Self {
            env: Arc::new(loft_config::process_env()),
            config: loft_config::Config::default(),
            flush_interval: Duration::from_millis(250),
            mcp_bridge: None,
            worktrees_dir: std::env::temp_dir().join("agentloft-worktrees"),
            chats_dir: std::env::temp_dir().join("agentloft-chats"),
            projects_dir: std::env::temp_dir().join("agentloft-projects"),
            config_file: None,
            attachments_dir: std::env::temp_dir().join("agentloft-attachments"),
            fetch: None,
            device_backends: Vec::new(),
            models_dir: std::env::temp_dir().join("agentloft-models"),
            voice_engine: None,
        }
    }
}

pub struct Core {
    store: Mutex<Store>,
    bus: broadcast::Sender<BusMsg>,
    harnesses: HashMap<String, Arc<dyn Harness>>,
    threads: Mutex<HashMap<ThreadId, ThreadHandle>>,
    /// In-progress items being streamed, overlaid on snapshots.
    live: Mutex<HashMap<ItemId, Item>>,
    terminals: terminals::Terminals,
    /// The daemon's own browser and its pages (ADR-0017).
    pages: pages::Pages,
    /// Simulators and emulators (ADR-0020).
    devices: Arc<devices::Devices>,
    /// Serializes worktree creation (two first messages racing).
    worktree_lock: tokio::sync::Mutex<()>,
    /// Which threads' pull requests are followed, and when each is due.
    pull_requests: pull_requests::PullRequests,
    /// Live config (starts as `opts.config`; replaced on reload).
    config: std::sync::RwLock<loft_config::Config>,
    /// `ListModels` answers per harness (probing Codex starts its CLI).
    models: Mutex<HashMap<String, Vec<loft_protocol::ModelInfo>>>,
    /// Harnesses whose `list_models` answered (with effort levels, which a
    /// session's own list lacks).
    models_listed: Mutex<std::collections::HashSet<String>>,
    /// Slash commands per (harness, folder), with when they were asked.
    commands: Mutex<CommandCache>,
    /// Worktree setup commands to run before a thread's first prompt.
    pending_setup: Mutex<HashMap<ThreadId, String>>,
    /// The worktree setup command the user allowed per project (by id),
    /// kept in `<home>/setup-approved.json`: a project's own config names
    /// it, so a cloned repository (or an agent editing that file) must not
    /// get a command run outside any sandbox unasked.
    setup_approved: Mutex<HashMap<String, String>>,
    /// For a thread whose worktree was just made again: a notice for the
    /// conversation and a note for the agent, with its next prompt.
    pending_notes: Mutex<HashMap<ThreadId, (String, String)>>,
    /// File lists per directory for `FindFiles`, briefly cached.
    file_index: Mutex<HashMap<PathBuf, FileIndex>>,
    /// Held while a directory is walked for `file_index`, so that the key
    /// presses that arrive meanwhile wait for that walk instead of each
    /// starting another.
    file_walk: tokio::sync::Mutex<()>,
    /// Bumped whenever the live config is replaced.
    config_rev: tokio::sync::watch::Sender<u64>,
    /// Model prices fetched from models.dev (the daemon refreshes them).
    price_catalog: std::sync::RwLock<Option<Arc<loft_config::pricing::Catalog>>>,
    /// Wakes the automation scheduler when an automation changes.
    automation_wake: Arc<tokio::sync::Notify>,
    /// Threads to carry on when their usage limit resets, and when that is
    /// (`limits.rs`).
    limit_resumes: Mutex<HashMap<ThreadId, i64>>,
    limit_wake: Arc<tokio::sync::Notify>,
    /// The id the next queued message gets.
    queued_ids: std::sync::atomic::AtomicU64,
    /// The agents' latest releases, per channel (`agent_updates.rs`).
    latest_releases: agent_updates::Latest,
    /// Harnesses whose CLI is being updated now.
    updating: Mutex<std::collections::HashSet<String>>,
    /// Voice input's models and the processes that run them.
    voice: loft_voice::Voice,
    opts: CoreOptions,
    // Scripts (D)
    /// Agents' browser scripts: each tab's `state`, and how many run.
    scripts: scripts::Scripts,
}

pub type CoreResult<T> = std::result::Result<T, ErrorBody>;

fn internal(e: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, e.to_string())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    pub fn new(store: Store, harnesses: Vec<Arc<dyn Harness>>, opts: CoreOptions) -> Arc<Self> {
        let (bus, _) = broadcast::channel(4096);
        if let Some(bridge) = &opts.mcp_bridge
            && let Err(err) = bridge.install_cli()
        {
            tracing::warn!(%err, "couldn't write agents' agent-device command");
        }
        let devices = Arc::new(devices::Devices::new(
            opts.device_backends.clone(),
            bus.clone(),
        ));
        let voice = loft_voice::Voice::new(loft_voice::VoiceOptions {
            models_dir: opts.models_dir.clone(),
            engine: opts.voice_engine.clone(),
            fake: opts
                .env
                .get("AGENTLOFT_FAKE_VOICE")
                .or(std::env::var("AGENTLOFT_FAKE_VOICE").ok().as_ref())
                .is_some_and(|v| v == "1"),
        });
        Arc::new(Self {
            devices,
            voice,
            store: Mutex::new(store),
            bus,
            harnesses: harnesses.into_iter().map(|h| (h.id().0, h)).collect(),
            threads: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            terminals: terminals::Terminals::default(),
            pages: pages::Pages::default(),
            worktree_lock: tokio::sync::Mutex::new(()),
            pull_requests: Default::default(),
            config: std::sync::RwLock::new(opts.config.clone()),
            models: Mutex::new(HashMap::new()),
            models_listed: Mutex::new(Default::default()),
            commands: Mutex::new(HashMap::new()),
            pending_setup: Mutex::new(HashMap::new()),
            setup_approved: Mutex::new(HashMap::new()),
            pending_notes: Mutex::new(HashMap::new()),
            file_index: Mutex::new(HashMap::new()),
            file_walk: tokio::sync::Mutex::new(()),
            config_rev: tokio::sync::watch::channel(0).0,
            price_catalog: std::sync::RwLock::new(None),
            automation_wake: Arc::new(tokio::sync::Notify::new()),
            limit_resumes: Mutex::new(HashMap::new()),
            limit_wake: Arc::new(tokio::sync::Notify::new()),
            // From the clock, so ids from before a restart aren't given again.
            queued_ids: std::sync::atomic::AtomicU64::new(now_ms().max(0) as u64),
            latest_releases: Default::default(),
            updating: Mutex::new(Default::default()),
            opts,
            // Scripts (D)
            scripts: scripts::Scripts::default(),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusMsg> {
        self.bus.subscribe()
    }

    /// Simulators and emulators (ADR-0020).
    pub fn devices(&self) -> &Arc<devices::Devices> {
        &self.devices
    }

    /// A subscription's first message for [`Topic::Devices`] or
    /// [`Topic::Device`].
    pub async fn device_snapshot(&self, topic: &Topic) -> CoreResult<Snapshot> {
        match topic {
            Topic::Devices => {
                let (devices, support) = self.devices.list().await;
                Ok(Snapshot::Devices { devices, support })
            }
            Topic::Device(id) => {
                let (device, frame, seq) = self.devices.snapshot(id).await?;
                Ok(Snapshot::Device { seq, device, frame })
            }
            _ => Err(ErrorBody::new(ErrorCode::BadRequest, "not a device topic")),
        }
    }

    /// Append events, update projections and broadcast — atomically ordered.
    pub(crate) fn commit(&self, events: Vec<Event>) -> CoreResult<Vec<EventEnvelope>> {
        if events.is_empty() {
            return Ok(vec![]);
        }
        let mut store = lock(&self.store);
        let envs = store.commit(events).map_err(internal)?;
        for env in &envs {
            let _ = self.bus.send(BusMsg::Event(Arc::new(env.clone())));
        }
        Ok(envs)
    }

    pub(crate) fn publish_delta(&self, delta: Delta) {
        let _ = self.bus.send(BusMsg::Delta(delta));
    }

    pub(crate) fn with_store<T>(&self, f: impl FnOnce(&mut Store) -> Result<T>) -> CoreResult<T> {
        f(&mut lock(&self.store)).map_err(internal)
    }

    pub(crate) fn live(&self) -> MutexGuard<'_, HashMap<ItemId, Item>> {
        lock(&self.live)
    }

    pub(crate) fn take_pending_setup(&self, thread: &ThreadId) -> Option<String> {
        lock(&self.pending_setup).remove(thread)
    }

    fn setup_approvals_file(&self) -> Option<PathBuf> {
        let home = self.opts().config_file.as_ref()?.parent()?;
        Some(home.join("setup-approved.json"))
    }

    /// Whether the user allowed exactly `cmd` as `project`'s worktree setup.
    pub(crate) fn setup_approved(&self, project: &ProjectId, cmd: &str) -> bool {
        let mut approved = lock(&self.setup_approved);
        if approved.is_empty()
            && let Some(file) = self.setup_approvals_file()
            && let Ok(text) = std::fs::read_to_string(file)
            && let Ok(saved) = serde_json::from_str::<HashMap<String, String>>(&text)
        {
            *approved = saved;
        }
        approved.get(project.as_str()).is_some_and(|c| c == cmd)
    }

    /// Remember that the user allowed `cmd` as `project`'s setup (until the
    /// project's config names another command).
    pub(crate) fn approve_setup(&self, project: &ProjectId, cmd: &str) {
        let mut approved = lock(&self.setup_approved);
        approved.insert(project.as_str().to_owned(), cmd.to_owned());
        if let Some(file) = self.setup_approvals_file()
            && let Ok(json) = serde_json::to_vec_pretty(&*approved)
        {
            let tmp = file.with_extension("json.tmp");
            if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &file))
            {
                tracing::warn!(%err, "couldn't save the setup command's approval");
            }
        }
    }

    pub(crate) fn take_pending_note(&self, thread: &ThreadId) -> Option<(String, String)> {
        lock(&self.pending_notes).remove(thread)
    }

    /// Terminal output from stream offset `from` to its current end.
    /// `None` when `from` has already scrolled out of the ring (re-snapshot).
    pub fn terminal_read_since(
        &self,
        id: &loft_protocol::TerminalId,
        from: u64,
    ) -> CoreResult<Option<(Vec<u8>, u64)>> {
        self.terminals.read_since(id, from)
    }

    /// The model prices fetched last, if any.
    pub fn price_catalog(&self) -> Option<Arc<loft_config::pricing::Catalog>> {
        self.price_catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_price_catalog(&self, catalog: loft_config::pricing::Catalog) {
        *self
            .price_catalog
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(catalog));
    }

    /// The current config (reloaded by the settings window).
    pub fn config(&self) -> loft_config::Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Changes whenever the config is reloaded (`borrow` is a counter).
    pub fn config_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.config_rev.subscribe()
    }

    /// What was asked in a turn, to ask it again: its messages (the prompt
    /// and any sent into it while it ran) and the images still on disk.
    fn turn_prompt(
        &self,
        thread_id: &ThreadId,
        turn_id: &loft_protocol::TurnId,
    ) -> CoreResult<(String, Vec<loft_protocol::AttachmentRef>)> {
        let items = self.with_store(|s| s.items(thread_id))?;
        let mut texts = Vec::new();
        let mut images = Vec::new();
        for item in items {
            if item.turn_id.as_ref() != Some(turn_id) || item.parent_id.is_some() {
                continue;
            }
            if let loft_protocol::ItemBody::UserMessage { text, attachments } = item.body {
                if !text.trim().is_empty() {
                    texts.push(text);
                }
                images.extend(
                    attachments
                        .into_iter()
                        .filter(|a| std::path::Path::new(&a.path).is_file()),
                );
            }
        }
        if texts.is_empty() && images.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "that turn has no message to send again",
            ));
        }
        Ok((texts.join("\n\n"), images))
    }

    /// Validate and store uploaded images; the agent gets file paths.
    async fn store_attachments(
        &self,
        thread_id: &ThreadId,
        attachments: Vec<loft_protocol::Attachment>,
    ) -> CoreResult<Vec<loft_protocol::AttachmentRef>> {
        const MAX_COUNT: usize = 10;
        const MAX_BYTES: usize = 20 * 1024 * 1024;
        if attachments.len() > MAX_COUNT {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("at most {MAX_COUNT} attachments per message"),
            ));
        }
        let dir = self.opts.attachments_dir.join(thread_id.as_str());
        let mut stored = Vec::new();
        for a in attachments {
            let ext = match a.mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                other => {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        format!("unsupported attachment type `{other}` (images only)"),
                    ));
                }
            };
            if a.data.0.len() > MAX_BYTES {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    format!("`{}` is larger than 20 MB", a.name),
                ));
            }
            let path = dir.join(format!("{}.{ext}", uuid::Uuid::now_v7()));
            let (d, p, bytes) = (dir.clone(), path.clone(), a.data.0);
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&d)?;
                std::fs::write(&p, bytes)
            })
            .await
            .map_err(internal)?
            .map_err(internal)?;
            stored.push(loft_protocol::AttachmentRef {
                name: a.name,
                mime: a.mime,
                path: path.to_string_lossy().into_owned(),
            });
        }
        Ok(stored)
    }

    /// Every file under `dir` (gitignore-aware, ≤100k), cached for 15 s so
    /// typing `@…` doesn't rewalk the tree on each keystroke.
    async fn file_index(&self, dir: PathBuf) -> CoreResult<Arc<Vec<String>>> {
        const FRESH: Duration = Duration::from_secs(15);
        let cached = |dir: &PathBuf| {
            lock(&self.file_index)
                .get(dir)
                .filter(|(at, _)| at.elapsed() < FRESH)
                .map(|(_, files)| files.clone())
        };
        if let Some(files) = cached(&dir) {
            return Ok(files);
        }
        let _walking = self.file_walk.lock().await;
        // Whoever held the lock may have walked this very directory.
        if let Some(files) = cached(&dir) {
            return Ok(files);
        }
        let d = dir.clone();
        let files = Arc::new(
            tokio::task::spawn_blocking(move || workbench::walk_files(&d))
                .await
                .map_err(internal)?,
        );
        let mut index = lock(&self.file_index);
        // Lists nobody will be given again (every worktree is a directory of
        // its own, and a list can be 100k paths) are let go.
        index.retain(|_, (at, _)| at.elapsed() < FRESH);
        index.insert(dir, (std::time::Instant::now(), files.clone()));
        Ok(files)
    }

    /// The harness's CLI: the configured path, or its name on the login
    /// shell's PATH. `Err` names the binary that wasn't found.
    fn harness_bin(&self, harness: &HarnessId) -> Result<PathBuf, String> {
        let bin = match harness.as_str() {
            HarnessId::CLAUDE => "claude",
            HarnessId::CODEX => "codex",
            other => other,
        };
        let hcfg = harness_config(&self.config(), harness);
        let path = if hcfg.path != "auto" {
            Some(PathBuf::from(&hcfg.path)).filter(|p| p.exists())
        } else {
            let env = &self.opts.env;
            loft_config::which(bin, env.get("PATH").map(String::as_str).unwrap_or(""))
        };
        path.ok_or_else(|| bin.to_owned())
    }

    /// Resolve the harness's CLI (config path or login PATH), ask its
    /// version, and whether a newer release is out on the channel it was
    /// installed from.
    async fn probe_harness(&self, harness: &HarnessId) -> CoreResult<Response> {
        let path = self.harness_bin(harness);
        let env = self.opts.env.clone();
        let fetch = self.opts.fetch.clone();
        let latest = self.latest_releases.clone();
        let id = harness.as_str().to_owned();
        let status = tokio::task::spawn_blocking(move || {
            let path = match path {
                Ok(path) => path,
                Err(bin) => {
                    return loft_protocol::HarnessStatus {
                        installed: false,
                        path: None,
                        version: None,
                        error: Some(format!("`{bin}` was not found on your login shell's PATH")),
                        update: None,
                    };
                }
            };
            let out = std::process::Command::new(&path)
                .arg("--version")
                .env_clear()
                .envs(env.iter())
                .stdin(std::process::Stdio::null())
                .output();
            let (version, error) = match out {
                Ok(o) if o.status.success() => (
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .map(|l| l.trim().to_owned()),
                    None,
                ),
                Ok(o) => (
                    None,
                    Some(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
                ),
                Err(e) => (None, Some(e.to_string())),
            };
            let update = version
                .as_deref()
                .zip(fetch)
                .and_then(|(installed, fetch)| {
                    let install = agent_updates::locate(&id, &path, &env)?;
                    let newest = latest.get(&install.source, &fetch)?;
                    agent_updates::is_newer(&newest, installed).then(|| {
                        loft_protocol::HarnessUpdate {
                            version: newest,
                            command: install.command_line(),
                        }
                    })
                });
            loft_protocol::HarnessStatus {
                installed: true,
                path: Some(path.to_string_lossy().into_owned()),
                version,
                error,
                update,
            }
        })
        .await
        .map_err(internal)?;
        Ok(Response::HarnessStatus { status })
    }

    /// Install the harness CLI's latest release with the package manager it
    /// came from, then probe it again.
    async fn update_harness(&self, harness: &HarnessId) -> CoreResult<Response> {
        let failed = |msg: String| ErrorBody::new(ErrorCode::Internal, msg);
        let path = self
            .harness_bin(harness)
            .map_err(|bin| failed(format!("`{bin}` isn't installed")))?;
        let env = self.opts.env.clone();
        let install = agent_updates::locate(harness.as_str(), &path, &env).ok_or_else(|| {
            failed("Agentloft can't tell how this agent was installed; update it the way you installed it.".into())
        })?;
        let (program, args) = install.command.clone().ok_or_else(|| {
            failed("Agentloft can't run this agent's package manager; update it the way you installed it.".into())
        })?;
        {
            let mut updating = lock(&self.updating);
            if !updating.insert(harness.as_str().to_owned()) {
                return Err(failed("It's already being updated.".into()));
            }
        }
        struct Done<'a>(&'a Core, String);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                lock(&self.0.updating).remove(&self.1);
            }
        }
        let _done = Done(self, harness.as_str().to_owned());

        // The package manager's own folder first, so npm finds the Node
        // that owns the package.
        let mut path_var = env.get("PATH").cloned().unwrap_or_default();
        if let Some(dir) = program.parent() {
            path_var = format!(
                "{}{}{path_var}",
                dir.to_string_lossy(),
                loft_config::PATH_SEPARATOR
            );
        }
        tracing::info!(harness = %harness.as_str(), command = ?install.command_line(), "updating an agent");
        let child = tokio::process::Command::new(&program)
            .args(&args)
            .env_clear()
            .envs(env.iter())
            .env("PATH", path_var)
            // Homebrew and npm ask nothing, but nobody could answer.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(15 * 60), child)
            .await
            .map_err(|_| failed("The update took more than 15 minutes and was stopped.".into()))?
            .map_err(|e| failed(format!("Couldn't run `{}`: {e}", program.display())))?;
        if !out.status.success() {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = lines[lines.len().saturating_sub(12)..].join("\n");
            return Err(failed(format!(
                "`{}` failed:\n{tail}",
                install.command_line().unwrap_or_default()
            )));
        }
        self.probe_harness(harness).await
    }

    /// Models a live session reported (ACP agents list theirs at start).
    /// A session's list has no effort levels (ACP reports them only for
    /// the current model): those `list_models` found are kept.
    pub(crate) fn set_models(
        &self,
        harness: &HarnessId,
        mut models: Vec<loft_protocol::ModelInfo>,
    ) {
        let mut all = lock(&self.models);
        if let Some(known) = all.get(harness.as_str()) {
            for m in models.iter_mut().filter(|m| m.efforts.is_empty()) {
                if let Some(k) = known.iter().find(|k| k.id == m.id) {
                    m.efforts = k.efforts.clone();
                    m.default_effort = m.default_effort.take().or(k.default_effort.clone());
                }
            }
        }
        all.insert(harness.as_str().to_owned(), models);
    }

    async fn list_models(&self, harness: &HarnessId) -> CoreResult<Response> {
        let cached = lock(&self.models).get(harness.as_str()).cloned();
        if let Some(models) = &cached
            && lock(&self.models_listed).contains(harness.as_str())
        {
            return Ok(Response::Models {
                models: models.clone(),
            });
        }
        let h = self.harness(harness).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            )
        })?;
        let hcfg = harness_config(&self.config(), harness);
        let binary = (hcfg.path != "auto").then(|| PathBuf::from(&hcfg.path));
        let env = agent_env(&self.opts.env, &hcfg);
        let models = match tokio::task::spawn_blocking(move || h.list_models(&env, binary))
            .await
            .map_err(internal)?
        {
            Ok(models) => models,
            // What a session reported is better than nothing.
            Err(_) if cached.is_some() => {
                return Ok(Response::Models {
                    models: cached.unwrap_or_default(),
                });
            }
            Err(e) => return Err(ErrorBody::new(ErrorCode::Harness, format!("{e:#}"))),
        };
        // An empty answer isn't kept: ACP agents report theirs once a
        // session starts.
        if models.is_empty() {
            return Ok(Response::Models {
                models: cached.unwrap_or_default(),
            });
        }
        lock(&self.models).insert(harness.as_str().to_owned(), models.clone());
        lock(&self.models_listed).insert(harness.as_str().to_owned());
        Ok(Response::Models { models })
    }

    /// The commands `harness` offers in `cwd` (asked without a session;
    /// kept for a few minutes, as skills can change on disk).
    async fn list_commands(&self, harness: &HarnessId, cwd: String) -> CoreResult<Response> {
        const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(5 * 60);
        let key = (harness.as_str().to_owned(), cwd.clone());
        if let Some((at, commands)) = lock(&self.commands).get(&key)
            && at.elapsed() < FRESH_FOR
        {
            return Ok(Response::Commands {
                commands: commands.clone(),
            });
        }
        let h = self.harness(harness).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            )
        })?;
        let hcfg = harness_config(&self.config(), harness);
        let binary = (hcfg.path != "auto").then(|| PathBuf::from(&hcfg.path));
        let env = agent_env(&self.opts.env, &hcfg);
        let commands = tokio::task::spawn_blocking(move || h.list_commands(&env, binary, &cwd))
            .await
            .map_err(internal)?
            .map_err(|e| ErrorBody::new(ErrorCode::Harness, format!("{e:#}")))?;
        if !commands.is_empty() {
            lock(&self.commands).insert(key, (std::time::Instant::now(), commands.clone()));
        }
        Ok(Response::Commands { commands })
    }

    fn reload_config(&self) -> CoreResult<Response> {
        let Some(file) = &self.opts.config_file else {
            return Ok(Response::Ok);
        };
        let config = loft_config::Config::load_file(file)
            .map_err(|e| ErrorBody::new(ErrorCode::BadRequest, format!("{e:#}")))?;
        self.voice.unload_after_changed(config.voice.unload_after());
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        self.config_rev.send_modify(|rev| *rev += 1);
        // Binary paths may have changed.
        lock(&self.models).clear();
        tracing::info!("config reloaded");
        Ok(Response::Ok)
    }

    /// The environment agents and tools run with (the login shell's).
    pub fn env(&self) -> Arc<HashMap<String, String>> {
        self.opts().env.clone()
    }

    pub(crate) fn opts(&self) -> &CoreOptions {
        &self.opts
    }

    /// An id for a message joining a thread's queue.
    pub(crate) fn next_queued_id(&self) -> u64 {
        self.queued_ids
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn harness(&self, id: &HarnessId) -> Option<Arc<dyn Harness>> {
        self.harnesses.get(id.as_str()).cloned()
    }

    // -- startup ------------------------------------------------------------

    /// Crash recovery (PLAN §5.6): turns left running become interrupted,
    /// open approvals expire, streaming items are closed, threads go idle.
    pub fn recover(&self) -> CoreResult<usize> {
        let (turns, requests, items, threads) = self.with_store(|s| {
            Ok((
                s.running_turns()?,
                s.open_requests()?,
                s.in_progress_items()?,
                s.threads()?,
            ))
        })?;
        let now = now_ms();
        let mut events = Vec::new();
        for mut item in items {
            item.status = ItemStatus::Completed;
            item.updated_at_ms = now;
            events.push(Event::ItemUpserted { item });
        }
        for mut request in requests {
            request.status = RequestStatus::Expired;
            events.push(Event::RequestUpdated { request });
        }
        let recovered_turns = turns.len();
        for mut turn in turns {
            turn.status = TurnStatus::Interrupted;
            turn.ended_at_ms = Some(now);
            turn.error = Some("Agentloft daemon restarted during this turn".into());
            events.push(Event::TurnUpdated { turn });
        }
        for mut thread in threads {
            let busy = matches!(
                thread.status,
                ThreadStatus::Running | ThreadStatus::WaitingApproval
            );
            // Background work died with the agent's process.
            let waiting = !thread.waiting_on.is_empty();
            // A tab handed to a person closed with the browser.
            let handed = thread.handover.is_some();
            if busy {
                thread.status = ThreadStatus::Idle;
                thread.updated_at_ms = now;
            }
            if busy || waiting || handed {
                thread.waiting_on.clear();
                thread.handover = None;
                events.push(Event::ThreadUpdated { thread });
            }
        }
        self.commit(events)?;
        Ok(recovered_turns)
    }

    // -- queries ------------------------------------------------------------

    /// More of a thread than its snapshot carried, as live as the snapshot.
    fn thread_items(
        &self,
        thread_id: &ThreadId,
        page: loft_protocol::ItemsPage,
    ) -> CoreResult<Response> {
        let all = self.with_store(|s| {
            s.thread(thread_id)?
                .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
            s.items(thread_id)
        })?;
        let (mut items, older) = match page {
            loft_protocol::ItemsPage::Steps { of } => (item_pages::steps(all, &of), false),
            loft_protocol::ItemsPage::Before { ordinal } => {
                let page = item_pages::page(all, Some(ordinal));
                (page.items, page.older)
            }
        };
        let live = self.live();
        for item in items.iter_mut() {
            if let Some(l) = live.get(&item.id) {
                *item = l.clone();
            }
        }
        Ok(Response::Items { items, older })
    }

    pub fn snapshot(&self, topic: &Topic) -> CoreResult<Snapshot> {
        if let Topic::Terminal(id) = topic {
            let (terminal, replay, end) = self.terminals.snapshot(id)?;
            return Ok(Snapshot::Terminal {
                seq: end,
                terminal,
                replay: replay.into(),
            });
        }
        if matches!(topic, Topic::Devices | Topic::Device(_)) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "devices are snapshotted with `device_snapshot`",
            ));
        }
        if let Topic::Page(id) = topic {
            let (page, frame, seq) = self.pages.snapshot(id)?;
            return Ok(Snapshot::Page { seq, page, frame });
        }
        // Read under the store lock so `seq` is consistent with the data.
        let snap = self.with_store(|s| {
            let seq = s.max_seq()?;
            Ok(match topic {
                Topic::Workspace => Some(Snapshot::Workspace {
                    seq,
                    projects: s.projects()?,
                    threads: s.threads()?,
                    automations: s.automations()?,
                }),
                Topic::Terminal(_) | Topic::Page(_) | Topic::Devices | Topic::Device(_) => None,
                Topic::Thread(id) => match s.thread(id)? {
                    None => None,
                    Some(thread) => {
                        let page = item_pages::page(s.items(id)?, None);
                        Some(Snapshot::Thread {
                            seq,
                            queue: s.queue(id)?,
                            turns: s.turns(id)?,
                            items: page.items,
                            requests: s.requests(id)?,
                            thread,
                            older: page.older,
                            outline: page.outline,
                        })
                    }
                },
            })
        })?;
        let mut snap =
            snap.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        if let Snapshot::Thread { items, .. } = &mut snap {
            let live = self.live();
            for item in items.iter_mut() {
                if let Some(l) = live.get(&item.id) {
                    *item = l.clone();
                }
            }
        }
        Ok(snap)
    }

    // -- requests -----------------------------------------------------------

    /// Requests that must run in arrival order on the connection (keystrokes
    /// to a terminal). Returns `None` for everything else.
    pub fn handle_inline(&self, req: &Request) -> Option<CoreResult<Response>> {
        match req {
            Request::TerminalInput { id, data } => {
                Some(self.terminals.input(id, &data.0).map(|_| Response::Ok))
            }
            Request::TerminalResize { id, cols, rows } => Some(
                self.terminals
                    .resize(id, *cols, *rows)
                    .map(|_| Response::Ok),
            ),
            // Keys and clicks for a page, likewise.
            Request::PageInput { id, input } => {
                Some(self.pages.input(id, input.clone()).map(|_| Response::Ok))
            }
            _ => None,
        }
    }

    /// The latest picture of a page, if newer than `seq`.
    pub fn page_frame_after(
        &self,
        id: &loft_protocol::PageId,
        seq: u64,
    ) -> Option<loft_protocol::PageFrame> {
        self.pages.frame_after(id, seq)
    }

    pub fn page_info(&self, id: &loft_protocol::PageId) -> Option<loft_protocol::PageInfo> {
        self.pages.info(id)
    }

    /// A viewer started or stopped watching a page.
    pub async fn page_watching(&self, id: &loft_protocol::PageId, joined: bool) {
        self.pages.watching(id, joined).await;
    }

    /// Connection `conn` just used the page: what a view asks for next
    /// (say this, copy that) is for whoever is on that connection. `hears`:
    /// it plays sound on this computer ([`loft_protocol::SOUND`]).
    pub fn page_touched(&self, id: &loft_protocol::PageId, conn: u64, hears: bool) {
        self.pages.touched(id, conn, hears);
    }

    /// Connection `conn`, which plays sound on this computer, started or
    /// stopped watching a page: the page may be heard while one does.
    pub async fn page_hearing(&self, id: &loft_protocol::PageId, conn: u64, on: bool) {
        self.pages.hearing(id, conn, on).await;
    }

    /// The agent wrote more of a view, or finished it: its page, if one is
    /// open, shows it.
    pub(crate) fn view_written(
        self: &Arc<Self>,
        thread: &ThreadId,
        item: &ItemId,
        html: String,
        done: bool,
    ) {
        let (core, thread, item) = (self.clone(), thread.clone(), item.clone());
        tokio::spawn(async move {
            core.pages.update_view(&thread, &item, html, done).await;
        });
    }

    /// Where the daemon's browser keeps its profile, and which browser.
    fn browser_setup(&self) -> CoreResult<(PathBuf, Option<String>)> {
        let profile = self
            .opts()
            .config_file
            .as_ref()
            .and_then(|f| f.parent())
            .map(|home| home.join("browser").join("profile"))
            .ok_or_else(|| {
                ErrorBody::new(ErrorCode::Unsupported, "The browser isn't available here.")
            })?;
        Ok((profile, self.config().browser.executable))
    }

    /// How an agent's browser commands run for `thread`.
    fn drive_options(&self, thread: Option<&Thread>) -> pages::DriveOptions {
        let config = self.config();
        // Its workspace: where it works (a worktree, say), and its
        // project's folder.
        let project_root = thread
            .and_then(|t| self.with_store(|s| s.project(&t.project_id)).ok().flatten())
            .map(|p| p.root_path);
        let file_roots = thread
            .map(|t| t.cwd.clone())
            .into_iter()
            .chain(project_root)
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .collect();
        let any_local_file = thread.is_some_and(|t| {
            self.mode_or_default(&t.harness, t.permission_mode)
                == loft_protocol::PermissionMode::FullAccess
        });
        pages::DriveOptions {
            search_template: config.browser.search_template(),
            human_input: config.browser.human_input(),
            // Uploads come from the thread's own workspace (or the
            // temporary folder); local pages from the workspace.
            file_roots,
            any_local_file,
        }
    }

    /// [`Core::handle`] for a client on this computer (the Unix socket),
    /// which may also show local files in the daemon's browser: what it
    /// opens is its own user's. A browser session never may (ADR-0017 §4).
    pub async fn handle_local(self: &Arc<Self>, req: Request) -> CoreResult<Response> {
        match req {
            Request::PageOpen {
                thread_id,
                url: Some(url),
                width,
                height,
                scale,
                tab,
                new_tab,
            } if pages::is_file_url(&url) => {
                let open = Request::PageOpen {
                    thread_id,
                    url: None,
                    width,
                    height,
                    scale,
                    tab,
                    new_tab,
                };
                let Response::Page { page } = self.handle(open).await? else {
                    return Err(ErrorBody::new(ErrorCode::Internal, "no page"));
                };
                self.pages.open_file(&page.id, &url).await?;
                let page = self.pages.info(&page.id).unwrap_or(page);
                Ok(Response::Page { page })
            }
            Request::PageNavigate {
                id,
                nav: loft_protocol::PageNav::Url { url },
            } if pages::is_file_url(&url) && !self.pages.is_view(&id) => {
                self.pages.open_file(&id, &url).await?;
                Ok(Response::Ok)
            }
            other => self.handle(other).await,
        }
    }

    /// The page that draws the view in `item` (a `show_ui` call of
    /// `thread`), opened if it isn't.
    async fn open_view(
        self: &Arc<Self>,
        thread: &ThreadId,
        item: &ItemId,
        width: u32,
        scale: f32,
        theme: Option<loft_protocol::ViewTheme>,
    ) -> CoreResult<loft_protocol::PageInfo> {
        // As it is being written, if it still is; else as it was kept.
        let live = self.live().get(item).cloned();
        let found = match live {
            Some(found) => Some(found),
            None => self.with_store(|s| s.item(thread, item))?,
        };
        let not_a_view = || ErrorBody::new(ErrorCode::NotFound, "That isn't a view.");
        let found = found
            .filter(|i| &i.thread_id == thread)
            .ok_or_else(not_a_view)?;
        let loft_protocol::ItemBody::ToolCall { tool, input, .. } = &found.body else {
            return Err(not_a_view());
        };
        if !loft_view::is_tool(tool) {
            return Err(not_a_view());
        }
        let done = input[loft_view::PARTIAL] != true;
        let html = input["html"].as_str().unwrap_or_default().to_owned();
        let state = self
            .with_store(|s| s.view_state(thread, item))?
            .unwrap_or(serde_json::Value::Null);
        let keep: pages::StateKeeper = {
            let core = Arc::downgrade(self);
            Arc::new(move |thread, item, state| {
                if let Some(core) = core.upgrade()
                    && let Err(err) = core.with_store(|s| s.set_view_state(thread, item, state))
                {
                    tracing::warn!(err = %err.message, "keeping a view's state");
                }
            })
        };
        let (profile, executable) = self.browser_setup()?;
        self.pages
            .open_view(
                pages::ViewRequest {
                    thread: thread.clone(),
                    item: item.clone(),
                    html,
                    done,
                    width,
                    scale,
                    theme,
                    state,
                    keep,
                },
                executable.as_deref(),
                &profile,
                self.bus.clone(),
            )
            .await
    }

    /// An agent's `show_ui` call: find (or make) its item, load the page
    /// once, and say what went wrong in it, if anything did.
    async fn show_view(
        self: &Arc<Self>,
        thread_id: ThreadId,
        title: Option<String>,
        html: String,
    ) -> CoreResult<Response> {
        let view = loft_view::parse(&serde_json::json!({ "title": title, "html": html }))
            .map_err(|problem| ErrorBody::new(ErrorCode::BadRequest, problem))?;
        // The harness reports the call on its own channel; give that a
        // moment to arrive before deciding it never will.
        let mut item_id = None;
        for attempt in 0..3 {
            let make = attempt == 2;
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let (title, html) = (view.title.clone(), view.html.clone());
            let reply = self
                .thread_call(&thread_id, |reply| ThreadCmd::ShowView {
                    title,
                    html,
                    make,
                    reply,
                })
                .await?;
            if let Response::Json { value } = reply
                && let Some(id) = value["item_id"].as_str()
            {
                item_id = Some(ItemId(id.to_owned()));
                break;
            }
        }
        let item_id = item_id.ok_or_else(|| internal("the view has no item"))?;
        // Load it with nobody looking, to tell the agent whether it runs.
        let (problems, unchecked) = match self
            .open_view(&thread_id, &item_id, UNWATCHED_VIEW_WIDTH, 2.0, None)
            .await
        {
            Ok(_) => (
                self.pages
                    .view_problems(&thread_id, &item_id, VIEW_CHECK)
                    .await,
                None,
            ),
            Err(err) => (Vec::new(), Some(err.message)),
        };
        Ok(Response::Json {
            value: serde_json::json!({
                "item_id": item_id.0,
                "problems": problems,
                "unchecked": unchecked,
            }),
        })
    }

    pub async fn handle(self: &Arc<Self>, req: Request) -> CoreResult<Response> {
        match req {
            Request::Ping => Ok(Response::Pong),
            Request::AddProject { path, name } => self.add_project(path, name),
            Request::CreateChat { harness } => self.create_chat(harness),
            Request::CreateProject {
                parent,
                name,
                github,
            } => self.create_project(parent, name, github).await,
            Request::GitHubLogin => {
                let env = self.opts().env.clone();
                let login = worktrees::blocking(move || Ok(loft_git::github_login(&env)))
                    .await?
                    .unwrap_or_default();
                Ok(Response::Text { text: login })
            }
            Request::RenameProject { project_id, name } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty name"));
                }
                let mut project = self
                    .with_store(|s| s.project(&project_id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
                project.name = name;
                self.commit(vec![Event::ProjectUpdated {
                    project: project.clone(),
                }])?;
                Ok(Response::Project { project })
            }
            Request::RemoveProject { project_id } => self.remove_project(&project_id).await,
            Request::GetSettings => self.get_settings(),
            Request::UsageReport {
                utc_offset_min,
                from_day,
                to_day,
            } => self.usage_report(utc_offset_min, from_day, to_day),
            Request::ChangeSetting { key, value, from } => {
                if let Some(from) = &from {
                    self.check_agent_setting(from, &key, &value)?;
                    let shown = match &value {
                        serde_json::Value::Null => "its default".to_owned(),
                        v => agent_limits::shorten(&v.to_string(), 200),
                    };
                    self.ask_agent(
                        from,
                        format!("change the setting `{key}` to {shown}"),
                        "The agent asked to change one of Agentloft's settings. It stays \
                         changed after this turn."
                            .into(),
                    )
                    .await?;
                }
                self.change_setting(&key, &value)
            }
            Request::SaveTheme { toml, select, id } => {
                self.save_theme(&toml, select, id.as_deref())
            }
            Request::SetProjectIcon { project_id, icon } => {
                let mut project = self
                    .with_store(|s| s.project(&project_id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
                let icon = icon.map(|i| i.trim().to_owned()).filter(|i| !i.is_empty());
                // An image from elsewhere (an agent's pick) is copied in,
                // so it stays put.
                let icon = match (
                    icon,
                    self.opts().config_file.as_ref().and_then(|f| f.parent()),
                ) {
                    (Some(src), Some(home))
                        if !already_an_icon(std::path::Path::new(&src), home) =>
                    {
                        let src = expand_tilde(&src);
                        if !src.is_file() {
                            return Err(ErrorBody::new(
                                ErrorCode::BadRequest,
                                format!("{} isn't a file", src.display()),
                            ));
                        }
                        // It becomes readable to browser sessions: an image,
                        // never any other file named as one.
                        if !loft_config::icons::drawable(&src) {
                            return Err(ErrorBody::new(
                                ErrorCode::BadRequest,
                                format!(
                                    "{} isn't an image (SVG, PNG, ICO, JPEG, WebP or GIF under 2 MB)",
                                    src.display()
                                ),
                            ));
                        }
                        let ext = src
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("png")
                            .to_ascii_lowercase();
                        let dir = home.join("icons");
                        let dest = dir.join(format!("{}-{}.{ext}", project_id.as_str(), now_ms()));
                        std::fs::create_dir_all(&dir)
                            .and_then(|_| std::fs::copy(&src, &dest))
                            .map_err(internal)?;
                        Some(dest.to_string_lossy().into_owned())
                    }
                    (icon, _) => icon,
                };
                project.icon = icon;
                self.commit(vec![Event::ProjectUpdated {
                    project: project.clone(),
                }])?;
                Ok(Response::Project { project })
            }
            Request::SetProjectDefaults {
                project_id,
                defaults,
            } => {
                let mut project = self
                    .with_store(|s| s.project(&project_id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
                project.defaults = defaults;
                self.commit(vec![Event::ProjectUpdated {
                    project: project.clone(),
                }])?;
                Ok(Response::Project { project })
            }
            Request::CloneProject {
                parent,
                source,
                github,
            } => self.clone_project(parent, source, github).await,
            Request::ListProjects => {
                let (projects, threads) = self.with_store(|s| Ok((s.projects()?, s.threads()?)))?;
                let chats = std::fs::canonicalize(&self.opts().chats_dir)
                    .unwrap_or_else(|_| self.opts().chats_dir.clone());
                Ok(Response::Json {
                    value: serde_json::Value::Array(
                        projects
                            .into_iter()
                            .map(|p| {
                                let count = threads
                                    .iter()
                                    .filter(|t| {
                                        t.project_id == p.id && t.status != ThreadStatus::Archived
                                    })
                                    .count();
                                serde_json::json!({
                                    "id": p.id, "name": p.name, "path": p.root_path,
                                    "chats": std::path::Path::new(&p.root_path) == chats,
                                    "threads": count,
                                    "icon": p.icon,
                                    "defaults": p.defaults,
                                })
                            })
                            .collect(),
                    ),
                })
            }
            Request::CreateThread {
                project_id,
                harness,
                title,
            } => self.create_thread(project_id, harness, title),
            Request::SendMessage {
                thread_id,
                text,
                attachments,
            } => {
                if text.trim().is_empty() && attachments.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty message"));
                }
                let attachments = self.store_attachments(&thread_id, attachments).await?;
                self.ensure_workspace(&thread_id, &text).await?;
                self.thread_call(&thread_id, |reply| ThreadCmd::SendMessage {
                    text,
                    attachments,
                    reply,
                })
                .await
            }
            Request::Steer {
                thread_id,
                text,
                attachments,
            } => {
                if text.trim().is_empty() && attachments.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty message"));
                }
                let attachments = self.store_attachments(&thread_id, attachments).await?;
                self.ensure_workspace(&thread_id, &text).await?;
                self.thread_call(&thread_id, |reply| ThreadCmd::Steer {
                    text,
                    attachments,
                    reply,
                })
                .await
            }
            Request::Enqueue {
                thread_id,
                text,
                attachments,
            } => {
                if text.trim().is_empty() && attachments.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty message"));
                }
                let attachments = self.store_attachments(&thread_id, attachments).await?;
                self.ensure_workspace(&thread_id, &text).await?;
                self.thread_call(&thread_id, |reply| ThreadCmd::Enqueue {
                    text,
                    attachments,
                    reply,
                })
                .await
            }
            Request::Dequeue { thread_id, id } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::Dequeue { id, reply })
                    .await
            }
            Request::MoveQueued { thread_id, id, to } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::MoveQueued { id, to, reply })
                    .await
            }
            Request::SendQueued { thread_id, id } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SendQueued { id, reply })
                    .await
            }
            Request::ResumeAtReset { thread_id, on } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::ResumeAtReset { on, reply })
                    .await
            }
            Request::Interrupt { thread_id } => self.interrupt(&thread_id).await,
            Request::AgentInfo { thread_id, kind } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::AgentInfo { kind, reply })
                    .await
            }
            Request::StopSubagent { thread_id, item_id } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::StopSubagent {
                    item_id,
                    reply,
                })
                .await
            }
            Request::RespondRequest {
                thread_id,
                request_id,
                decision,
                reply: answer,
            } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::Respond {
                    request_id,
                    decision,
                    answer,
                    reply,
                })
                .await
            }
            Request::SetPermissionMode { thread_id, mode } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SetPermissionMode {
                    mode,
                    reply,
                })
                .await
            }
            Request::Shutdown { .. } => Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "shutdown is handled by the daemon",
            )),
            Request::WebStatus
            | Request::WebRotateToken
            | Request::WebRevokeSession { .. }
            | Request::WebRevokeAllSessions
            | Request::WebLinkStart
            | Request::WebLinkCancel
            | Request::WebUnlink
            | Request::WebSync => Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "web access is handled by the daemon",
            )),
            Request::UiFiles { known } => Ok(Response::UiFiles {
                files: self.ui_files(known.as_deref(), false)?.map(Box::new),
            }),
            Request::PutUiState { name, json } => self.put_ui_state(&name, &json),
            Request::DeleteTheme { id } => self.delete_theme(&id),
            Request::ProjectIcon { project_id } => {
                let core = self.clone();
                let file = tokio::task::spawn_blocking(move || core.project_icon(&project_id))
                    .await
                    .map_err(internal)??;
                Ok(Response::File { file })
            }
            Request::ProjectActions { project_id } => {
                let core = self.clone();
                let actions =
                    tokio::task::spawn_blocking(move || core.project_actions(&project_id))
                        .await
                        .map_err(internal)??;
                Ok(Response::Actions { actions })
            }
            Request::ReadPath {
                path,
                thread_id,
                offset,
            } => {
                let core = self.clone();
                let file = tokio::task::spawn_blocking(move || {
                    core.read_path(&path, thread_id.as_ref(), false, offset)
                })
                .await
                .map_err(internal)??;
                Ok(Response::File { file })
            }
            Request::FilesExist { paths, thread_id } => {
                let core = self.clone();
                let files = tokio::task::spawn_blocking(move || {
                    core.files_exist(&paths, thread_id.as_ref(), false)
                })
                .await
                .map_err(internal)?;
                Ok(Response::FilesExist { files })
            }
            Request::ListDir { root, rel_path } => {
                let core = self.clone();
                let entries = tokio::task::spawn_blocking(move || core.list_dir(&root, &rel_path))
                    .await
                    .map_err(internal)??;
                Ok(Response::DirListing { entries })
            }
            Request::ReadFile {
                root,
                rel_path,
                offset,
            } => {
                let core = self.clone();
                let file =
                    tokio::task::spawn_blocking(move || core.read_file(&root, &rel_path, offset))
                        .await
                        .map_err(internal)??;
                Ok(Response::File { file })
            }
            Request::GetDiff {
                thread_id,
                scope,
                from,
            } => {
                if let Some(from) = &from {
                    self.check_agent_reads(from, &thread_id)?;
                }
                Ok(Response::Diff {
                    diff: self.diff(&thread_id, scope).await?,
                })
            }
            Request::ThreadItems { thread_id, page } => self.thread_items(&thread_id, page),
            Request::TerminalOpen {
                root,
                cols,
                rows,
                key,
                command,
            } => {
                let command = command.filter(|c| !c.trim().is_empty());
                if let Some(terminal) = key
                    .as_deref()
                    .and_then(|k| self.terminals.reattach(k, cols, rows))
                {
                    // Its shell is live: the command goes in only if what
                    // it ran before has ended.
                    if let Some(command) = &command {
                        self.terminals.run_if_idle(&terminal.id, command);
                    }
                    return Ok(Response::Terminal { terminal });
                }
                let dir = self.root_dir(&root)?;
                let terminal = self.terminals.open(
                    &dir,
                    (cols, rows),
                    key,
                    command,
                    &self.opts.env,
                    self.bus.clone(),
                )?;
                Ok(Response::Terminal { terminal })
            }
            Request::TerminalInput { id, data } => {
                self.terminals.input(&id, &data.0)?;
                Ok(Response::Ok)
            }
            Request::TerminalResize { id, cols, rows } => {
                self.terminals.resize(&id, cols, rows)?;
                Ok(Response::Ok)
            }
            Request::ListTerminals { key_prefix } => Ok(Response::Terminals {
                terminals: self.terminals.list_prefix(&key_prefix),
            }),
            Request::TerminalClose { id } => {
                self.terminals.close(&id)?;
                Ok(Response::Ok)
            }
            Request::PageOpen {
                thread_id,
                url,
                width,
                height,
                scale,
                tab,
                new_tab,
            } => {
                // The thread has to be one of ours.
                self.with_store(|s| s.thread(&thread_id))?
                    .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "no such thread"))?;
                let (profile, executable) = self.browser_setup()?;
                let page = self
                    .pages
                    .open(
                        thread_id,
                        pages::Which::asked(tab, new_tab),
                        url,
                        width,
                        height,
                        scale,
                        executable.as_deref(),
                        &profile,
                        self.bus.clone(),
                    )
                    .await?;
                Ok(Response::Page { page })
            }
            Request::DeviceList => {
                let (devices, support) = self.devices.refresh().await;
                Ok(Response::Devices { devices, support })
            }
            Request::DeviceBoot {
                id,
                thread_id,
                wait: false,
            } => {
                self.devices.boot(&id, thread_id.as_ref()).await?;
                Ok(Response::Ok)
            }
            Request::DeviceBoot {
                id,
                thread_id,
                wait: true,
            } => Ok(Response::Device {
                device: self
                    .devices
                    .boot_and_wait(&id, thread_id.as_ref(), devices::BOOT_WAIT)
                    .await?,
            }),
            Request::DeviceShutdown { id } => {
                self.devices.shutdown(&id).await?;
                Ok(Response::Ok)
            }
            Request::DeviceInput {
                id,
                input,
                thread_id,
            } => {
                self.devices.input(&id, input, thread_id.as_ref()).await?;
                Ok(Response::Ok)
            }
            Request::DeviceCapture { id, thread_id } => Ok(Response::Picture {
                png: self.devices.capture(&id, thread_id.as_ref()).await?.into(),
            }),
            Request::DeviceApp { id, app, thread_id } => {
                self.devices.app(&id, app, thread_id.as_ref()).await?;
                Ok(Response::Ok)
            }
            Request::DeviceSet { id, setting } => {
                self.devices.set(&id, setting).await?;
                Ok(Response::Ok)
            }
            Request::DeviceSettings { id } => Ok(Response::DeviceSettings {
                settings: self.devices.settings(&id).await?,
            }),
            Request::PageList { thread_id } => Ok(Response::Pages {
                pages: self.pages.tabs(&thread_id),
            }),
            Request::PageNavigate { id, nav } => {
                // A view has nowhere to go.
                if self.pages.is_view(&id) {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        "A view can't be sent to an address.",
                    ));
                }
                self.pages.navigate(&id, nav).await?;
                Ok(Response::Ok)
            }
            Request::ViewOpen {
                thread_id,
                item_id,
                width,
                scale,
                theme,
            } => {
                let page = self
                    .open_view(&thread_id, &item_id, width, scale, Some(*theme))
                    .await?;
                Ok(Response::Page { page })
            }
            Request::ViewTheme { id, theme } => {
                self.pages.theme_view(&id, &theme).await?;
                Ok(Response::Ok)
            }
            Request::ViewState { thread_id, item_id } => Ok(Response::Json {
                value: self
                    .with_store(|s| s.view_state(&thread_id, &item_id))?
                    .unwrap_or(serde_json::Value::Null),
            }),
            Request::ViewKeep {
                thread_id,
                item_id,
                state,
            } => {
                if state.to_string().len() > loft_view::MAX_STATE {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        "That is more than a view may ask to have remembered.",
                    ));
                }
                self.with_store(|s| s.set_view_state(&thread_id, &item_id, &state))?;
                Ok(Response::Ok)
            }
            Request::ShowView {
                thread_id,
                title,
                html,
            } => self.show_view(thread_id, title, html).await,
            Request::PageInput { id, input } => {
                self.pages.input(&id, input)?;
                Ok(Response::Ok)
            }
            Request::PageResize {
                id,
                width,
                height,
                scale,
            } => {
                self.pages.resize(&id, width, height, scale).await?;
                Ok(Response::Ok)
            }
            Request::PageSelection { id } => Ok(Response::Text {
                text: self.pages.selection(&id).await?,
            }),
            Request::PageClose { id } => {
                self.pages.close(&id).await?;
                Ok(Response::Ok)
            }
            Request::PageZoom { id, zoom } => {
                self.pages.zoom(&id, zoom).await?;
                Ok(Response::Ok)
            }
            Request::PageEmulate { id, device } => {
                self.pages.emulate(&id, device).await?;
                Ok(Response::Ok)
            }
            Request::PageClearData { id, cookies, cache } => {
                self.pages.clear_data(&id, cookies, cache).await?;
                Ok(Response::Ok)
            }
            Request::PageCapture { id } => Ok(Response::Picture {
                png: self.pages.capture(&id).await?.into(),
            }),
            Request::PageAnnotate { id, on } => {
                self.pages.annotate(&id, on).await?;
                Ok(Response::Ok)
            }
            Request::PageUnpin { id, n } => {
                self.pages.unpin(&id, n).await?;
                Ok(Response::Ok)
            }
            Request::PageHandoffDone { id } => {
                self.pages.handover_done(&id)?;
                Ok(Response::Ok)
            }
            Request::BrowserCall {
                thread_id,
                command,
                tab,
            } => self.browser_request(thread_id, command, tab).await,
            Request::SetModel { thread_id, model } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SetModel { model, reply })
                    .await
            }
            Request::SetEffort { thread_id, effort } => {
                self.thread_call(&thread_id, |reply| ThreadCmd::SetEffort { effort, reply })
                    .await
            }
            Request::SetThreadHarness { thread_id, harness } => {
                self.set_thread_harness(&thread_id, harness)
            }
            Request::SetThreadWorkspace {
                thread_id,
                worktree,
                base,
            } => self.set_thread_workspace(&thread_id, worktree, base),
            Request::ListBranches { project_id } => self.list_branches(&project_id).await,
            Request::MergeThread { thread_id, message } => {
                self.merge_thread(&thread_id, message).await
            }
            Request::ForkThread {
                thread_id,
                harness,
                share_workspace,
                at_turn,
            } => {
                self.fork_thread(&thread_id, harness, share_workspace, at_turn)
                    .await
            }
            Request::ReloadConfig => self.reload_config(),
            Request::ListModels { harness } => self.list_models(&harness).await,
            Request::ListCommands { harness, cwd } => self.list_commands(&harness, cwd).await,
            Request::ProbeHarness { harness } => self.probe_harness(&harness).await,
            Request::UpdateHarness { harness } => self.update_harness(&harness).await,
            Request::FindFiles { root, query, limit } => {
                let dir = self.root_dir(&root)?;
                let files = self.file_index(dir).await?;
                Ok(Response::Paths {
                    paths: workbench::fuzzy_find(&files, &query, limit.min(100) as usize),
                })
            }
            Request::CreatePullRequest { thread_id } => self.create_pull_request(&thread_id).await,
            Request::RefreshPullRequest { thread_id } => {
                self.refresh_pull_request(&thread_id).await
            }
            Request::RevertTurn { thread_id, turn_id } => {
                self.revert_turn(&thread_id, &turn_id).await
            }
            Request::RetryTurn { thread_id, turn_id } => {
                let (text, attachments) = self.turn_prompt(&thread_id, &turn_id)?;
                self.ensure_workspace(&thread_id, &text).await?;
                self.thread_call(&thread_id, |reply| ThreadCmd::SendMessage {
                    text,
                    attachments,
                    reply,
                })
                .await
            }
            Request::DeleteThread { thread_id } => self.delete_thread(&thread_id).await,
            Request::CleanUpWorktree { thread_id } => {
                self.clean_up_worktree(&thread_id, true).await
            }
            Request::RestoreBranch { thread_id } => self.restore_branch(&thread_id).await,
            Request::WorktreeLeftovers => self.worktree_leftovers().await,
            Request::RemoveWorktreeLeftovers { leftovers } => {
                self.remove_worktree_leftovers(leftovers).await
            }
            Request::AskOnce {
                prompt,
                harness,
                model,
            } => {
                self.ask_once(prompt, harness.unwrap_or_else(HarnessId::claude), model)
                    .await
            }
            Request::SetDraft { thread_id, text } => {
                let draft = (!text.trim().is_empty()).then_some(text);
                self.thread_call(&thread_id, |reply| ThreadCmd::SetDraft { draft, reply })
                    .await
            }
            Request::RenameThread { thread_id, title } => {
                let title = title.trim().to_owned();
                if title.is_empty() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "empty title"));
                }
                self.thread_call(&thread_id, |reply| ThreadCmd::Rename { title, reply })
                    .await
            }
            Request::UnarchiveThread { thread_id } => self.unarchive_thread(&thread_id).await,
            Request::ArchiveThread { thread_id, force } => {
                self.archive_thread(&thread_id, force).await
            }
            Request::SpawnThread {
                from,
                prompt,
                title,
                harness,
                model,
                effort,
                permission_mode,
                worktree,
                top_level,
                project,
                chat,
            } => {
                self.spawn_thread(forks::SpawnSpec {
                    from,
                    prompt,
                    title,
                    harness,
                    model,
                    effort,
                    permission_mode,
                    worktree,
                    top_level,
                    project,
                    chat,
                })
                .await
            }
            Request::ThreadOutcome { thread_id, from } => {
                if let Some(from) = &from {
                    self.check_agent_reads(from, &thread_id)?;
                }
                Ok(Response::Json {
                    value: self.thread_outcome(&thread_id)?,
                })
            }
            Request::SearchThreads { query, limit } => Ok(Response::SearchHits {
                hits: self.search_threads(&query, limit.min(500) as usize)?,
            }),
            Request::ListThreads {
                query,
                project_id,
                from,
            } => {
                let readable = match &from {
                    Some(from) => self.agent_reads(from)?,
                    None => None,
                };
                Ok(Response::Json {
                    value: self.list_threads_json(
                        query.as_deref(),
                        project_id.as_ref(),
                        readable.as_ref(),
                    )?,
                })
            }
            Request::ReadThread {
                thread_id,
                detail,
                from,
            } => {
                if let Some(from) = &from {
                    self.check_agent_reads(from, &thread_id)?;
                }
                Ok(Response::Text {
                    text: self.read_thread_text(&thread_id, detail)?,
                })
            }
            Request::AgentApproval {
                thread_id,
                title,
                reason,
            } => {
                self.ask_agent(&thread_id, title, reason).await?;
                Ok(Response::Ok)
            }
            Request::ListAutomations => Ok(Response::Json {
                value: self.automations_json()?,
            }),
            Request::SaveAutomation { id, spec, from } => {
                if let Some(from) = &from {
                    self.check_agent_automation(from, &spec)?;
                    let what = if id.is_some() { "change" } else { "create" };
                    self.ask_agent(
                        from,
                        format!("{what} the automation “{}”", spec.name),
                        format!(
                            "It starts a new thread on its schedule, with this prompt: {}",
                            agent_limits::shorten(&spec.prompt, 400)
                        ),
                    )
                    .await?;
                }
                self.save_automation(id, spec, from)
            }
            Request::SetAutomationEnabled { id, enabled } => {
                self.set_automation_enabled(&id, enabled)
            }
            Request::DeleteAutomation { id } => self.delete_automation(&id),
            Request::RunAutomation { id } => self.run_automation_now(&id).await,
            Request::VoiceStatus => self.voice_status(),
            Request::VoiceDownload { model } => self.voice_download(&model),
            Request::VoiceCancelDownload { model } => self.voice_cancel_download(&model),
            Request::VoiceRemoveModel { model } => self.voice_remove(&model).await,
            Request::VoiceUnload => self.voice_unload().await,
            Request::VoiceTranscribe {
                audio,
                language,
                model,
            } => self.voice_transcribe(audio.0, language, model).await,
            Request::VoiceCleanup {
                transcript,
                before,
                after,
            } => self.voice_cleanup(transcript, before, after).await,
            Request::VoiceLiveStart { language } => self.voice_live_start(language).await,
            Request::VoiceLiveFeed { id, audio } => self.voice_live_feed(id, audio.0).await,
            Request::VoiceLiveFinish { id, audio } => self.voice_live_finish(id, audio.0).await,
            Request::VoiceLiveCancel { id } => self.voice_live_cancel(id).await,
        }
    }

    fn add_project(&self, path: String, name: Option<String>) -> CoreResult<Response> {
        let path = expand_tilde(&path);
        let canonical = std::fs::canonicalize(&path).map_err(|e| {
            ErrorBody::new(ErrorCode::BadRequest, format!("{}: {e}", path.display()))
        })?;
        if !canonical.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        let root_path = canonical.to_string_lossy().into_owned();
        if let Some(project) = self.with_store(|s| s.project_by_path(&root_path))? {
            return Ok(Response::Project { project });
        }
        let project = Project {
            id: ProjectId::new(),
            name: name.unwrap_or_else(|| {
                canonical
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root_path.clone())
            }),
            root_path,
            created_at_ms: now_ms(),
            defaults: Default::default(),
            icon: None,
        };
        self.commit(vec![Event::ProjectAdded {
            project: project.clone(),
        }])?;
        Ok(Response::Project { project })
    }

    /// A thread without a project: it lives in the built-in "Chats" project
    /// and works in an empty folder of its own.
    /// Delete every thread of the project (stopping them, removing their
    /// worktrees and checkpoints), then its entry. Files stay on disk.
    async fn remove_project(self: &Arc<Self>, id: &ProjectId) -> CoreResult<Response> {
        if self.with_store(|s| s.project(id))?.is_none() {
            return Err(ErrorBody::new(ErrorCode::NotFound, "project not found"));
        }
        // Its automations first: they would start threads in it.
        let gone: Vec<Event> = self
            .with_store(|s| s.automations())?
            .into_iter()
            .filter(|a| &a.spec.project_id == id)
            .map(|a| Event::AutomationDeleted {
                automation_id: a.id,
            })
            .collect();
        self.commit(gone)?;
        let threads = self.with_store(|s| s.threads())?;
        for t in threads.iter().filter(|t| &t.project_id == id) {
            self.delete_thread(&t.id).await?;
        }
        self.commit(vec![Event::ProjectRemoved {
            project_id: id.clone(),
        }])?;
        Ok(Response::Ok)
    }

    async fn create_project(
        &self,
        parent: Option<String>,
        name: String,
        github: bool,
    ) -> CoreResult<Response> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the project needs a name",
            ));
        }
        let parent = match parent {
            Some(p) => expand_tilde(&p),
            None => self.opts().projects_dir.clone(),
        };
        // "Pinball Stats" → pinball-stats (-2, -3… if taken).
        let slug = worktrees::slug_or(&name, "project");
        let mut dir = parent.join(&slug);
        let mut n = 2;
        while dir.exists() {
            dir = parent.join(format!("{slug}-{n}"));
            n += 1;
        }
        let (at, env, title) = (dir.clone(), self.opts().env.clone(), name.clone());
        let committed = worktrees::blocking(move || {
            loft_git::init_repo(&at)?;
            loft_git::first_commit(&at, &title, &env)
        })
        .await?;
        let added = self.add_project(dir.to_string_lossy().into_owned(), Some(name))?;
        if github {
            let repo = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(slug);
            let (at, env) = (dir.clone(), self.opts().env.clone());
            if let Err(e) =
                worktrees::blocking(move || loft_git::publish_github(&at, &repo, committed, &env))
                    .await
            {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "The project was created, but publishing it to GitHub failed: {}",
                        e.message
                    ),
                ));
            }
        }
        Ok(added)
    }

    async fn clone_project(
        &self,
        parent: String,
        source: String,
        github: bool,
    ) -> CoreResult<Response> {
        let source = source.trim().to_owned();
        if source.is_empty() {
            return Err(ErrorBody::new(ErrorCode::BadRequest, "nothing to clone"));
        }
        let source = if github {
            let repo = source
                .trim_start_matches("https://github.com/")
                .trim_end_matches(".git")
                .trim_matches('/')
                .to_owned();
            // Never an option for `gh` or git (`--upload-pack=…`).
            if repo.split('/').count() != 2 || repo.starts_with('-') {
                return Err(ErrorBody::new(ErrorCode::BadRequest, "use owner/repo"));
            }
            loft_git::CloneSource::GitHub(repo)
        } else {
            loft_git::CloneSource::Url(source)
        };
        let dest = expand_tilde(&parent).join(source.name());
        let (at, env) = (dest.clone(), self.opts().env.clone());
        worktrees::blocking(move || loft_git::clone_repo(&source, &at, &env)).await?;
        self.add_project(dest.to_string_lossy().into_owned(), None)
    }

    /// The built-in "Chats" project (created on first use).
    pub(crate) fn chats_project(&self) -> CoreResult<Project> {
        let dir = self.opts().chats_dir.clone();
        std::fs::create_dir_all(&dir).map_err(internal)?;
        match self.add_project(dir.to_string_lossy().into_owned(), Some("Chats".into()))? {
            Response::Project { project } => Ok(project),
            _ => Err(ErrorBody::new(ErrorCode::Internal, "no chats project")),
        }
    }

    fn create_chat(&self, harness: HarnessId) -> CoreResult<Response> {
        let project = self.chats_project()?;
        let Response::Thread { mut thread } = self.create_thread(project.id, harness, None)? else {
            return Err(ErrorBody::new(ErrorCode::Internal, "no thread"));
        };
        let own = std::path::Path::new(&project.root_path).join(thread.id.as_str());
        std::fs::create_dir_all(&own).map_err(internal)?;
        thread.cwd = own.to_string_lossy().into_owned();
        thread.workspace = loft_protocol::ThreadWorkspace::Local;
        self.commit(vec![Event::ThreadUpdated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Thread { thread })
    }

    fn set_thread_harness(&self, id: &ThreadId, harness: HarnessId) -> CoreResult<Response> {
        if self.harness(&harness).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            ));
        }
        let (thread, turns) = self.with_store(|s| Ok((s.thread(id)?, s.turns(id)?)))?;
        let mut thread =
            thread.ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        if !turns.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "a thread keeps its harness once it has started; fork it instead",
            ));
        }
        // Drop any idle actor holding the old thread state.
        lock(&self.threads).remove(id);
        let (model, mode, effort) = thread_defaults(&self.config(), &harness);
        thread.harness = harness;
        thread.native_session_id = None;
        thread.model = model;
        thread.effort = effort;
        thread.permission_mode = mode;
        thread.updated_at_ms = now_ms();
        self.commit(vec![Event::ThreadUpdated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Thread { thread })
    }

    fn create_thread(
        &self,
        project_id: ProjectId,
        harness: HarnessId,
        title: Option<String>,
    ) -> CoreResult<Response> {
        let project = self
            .with_store(|s| s.project(&project_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "project not found"))?;
        // The project's own defaults come first (Settings ▸ Projects).
        let d = project.defaults.clone();
        let harness = d
            .agent
            .clone()
            .filter(|a| self.harness(a).is_some())
            .unwrap_or(harness);
        if self.harness(&harness).is_none() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                format!("unknown harness `{harness}`"),
            ));
        }
        let now = now_ms();
        let workspace = match d.worktree {
            Some(true)
                if loft_git::repo_root(std::path::Path::new(&project.root_path)).is_some() =>
            {
                loft_protocol::ThreadWorkspace::NewWorktree { base: None }
            }
            Some(_) => loft_protocol::ThreadWorkspace::Local,
            None => self.default_workspace(&project.root_path),
        };
        let (model, permission_mode, effort) = thread_defaults(&self.config(), &harness);
        // A model and effort set for the project's agent.
        let own = d.agent.as_ref() == Some(&harness);
        let model = d.model.clone().filter(|_| own).or(model);
        let effort = d.effort.clone().filter(|_| own).or(effort);
        let thread = Thread {
            id: ThreadId::new(),
            project_id,
            title: title.unwrap_or_else(|| "New thread".into()),
            harness,
            cwd: project.root_path,
            model,
            permission_mode,
            effort,
            native_session_id: None,
            status: ThreadStatus::Idle,
            created_at_ms: now,
            updated_at_ms: now,
            workspace,
            parent: None,
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: None,
            draft: None,
            waiting_on: Vec::new(),
            limit: None,
            automation_id: None,
            pull_request: None,
            landed: false,
            suggestion: None,
            handover: None,
        };
        self.commit(vec![Event::ThreadCreated {
            thread: thread.clone(),
        }])?;
        Ok(Response::Thread { thread })
    }

    /// Stop a thread's running turn, and the turns of the threads its agent
    /// started (and theirs): stopping an orchestrator stops its workers.
    /// Not the ones it started as threads of their own (`detached`).
    pub(crate) async fn interrupt(self: &Arc<Self>, thread_id: &ThreadId) -> CoreResult<Response> {
        let own = self
            .thread_call(thread_id, |reply| ThreadCmd::Interrupt { reply })
            .await;
        let threads = self.with_store(|s| s.threads())?;
        let mut stopped = false;
        let mut parents = vec![thread_id.clone()];
        while let Some(parent) = parents.pop() {
            for child in threads.iter().filter(|t| {
                t.parent.as_ref().is_some_and(|p| {
                    p.thread_id == parent
                        && p.kind == loft_protocol::ForkKind::Spawned
                        && !p.detached
                })
            }) {
                parents.push(child.id.clone());
                if matches!(
                    child.status,
                    ThreadStatus::Running | ThreadStatus::WaitingApproval
                ) {
                    stopped |= self
                        .thread_call(&child.id, |reply| ThreadCmd::Interrupt { reply })
                        .await
                        .is_ok();
                }
            }
        }
        // Nothing of its own to stop, but something it started was.
        if stopped { Ok(Response::Ok) } else { own }
    }

    async fn thread_call(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        make: impl FnOnce(oneshot::Sender<CoreResult<Response>>) -> ThreadCmd,
    ) -> CoreResult<Response> {
        let handle = self.thread_handle(thread_id)?;
        let (tx, rx) = oneshot::channel();
        handle
            .send(make(tx))
            .map_err(|_| internal("thread actor stopped"))?;
        rx.await
            .map_err(|_| internal("thread actor dropped request"))?
    }

    pub(crate) fn thread_handle(
        self: &Arc<Self>,
        thread_id: &ThreadId,
    ) -> CoreResult<ThreadHandle> {
        let mut threads = lock(&self.threads);
        if let Some(h) = threads.get(thread_id).filter(|h| !h.is_closed()) {
            return Ok(h.clone());
        }
        let thread = self
            .with_store(|s| s.thread(thread_id))?
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "thread not found"))?;
        let handle = thread::spawn(self.clone(), thread);
        threads.insert(thread_id.clone(), handle.clone());
        Ok(handle)
    }

    /// Stop all harness sessions and terminals (daemon shutdown).
    pub async fn shutdown(&self) {
        self.terminals.shutdown();
        self.pages.shutdown().await;
        let handles: Vec<ThreadHandle> = lock(&self.threads).drain().map(|(_, h)| h).collect();
        let mut waits = Vec::new();
        for h in handles {
            let (tx, rx) = oneshot::channel();
            if h.send(ThreadCmd::Shutdown { done: tx }).is_ok() {
                waits.push(rx);
            }
        }
        for w in waits {
            let _ = tokio::time::timeout(Duration::from_secs(10), w).await;
        }
    }

    /// Whether any agent is at work (upgrade gating): a thread has a
    /// running turn, or is waiting on work its agent left going in the
    /// background (subagents, shells), which would die with the daemon.
    pub fn is_busy(&self) -> bool {
        self.with_store(|s| {
            Ok(!s.running_turns()?.is_empty()
                || s.threads()?.iter().any(|t| !t.waiting_on.is_empty()))
        })
        .unwrap_or(true)
    }

    pub(crate) fn record_process(&self, pid: u32, kind: &str, thread: &ThreadId) {
        let _ = self.with_store(|s| s.record_process(pid, kind, Some(thread)));
    }

    pub(crate) fn forget_process(&self, pid: u32) {
        let _ = self.with_store(|s| s.forget_process(pid));
    }
}

/// `[harness.<id>]` settings for a harness (defaults when unknown).
impl Core {
    /// A one-off answer from an agent's CLI: no tools, no session, low
    /// effort (Claude `-p`, `codex exec`, `opencode run`, headless `gemini`).
    ///
    /// The prompt (the user's conversation) goes on the CLI's stdin, never
    /// its command line: every account on the computer can read a
    /// process's arguments (`/proc/<pid>/cmdline` on Linux, `ps`). Each of
    /// these reads its prompt from stdin when given none as an argument
    /// and stdin isn't a terminal (checked against Claude Code 2.1, Codex
    /// 0.170, OpenCode 2.3 and Gemini CLI's `isHeadlessMode`).
    pub(crate) async fn ask_once(
        &self,
        prompt: String,
        harness: HarnessId,
        model: Option<String>,
    ) -> CoreResult<Response> {
        let hcfg = harness_config(&self.config(), &harness);
        let env = agent_env(&self.opts.env, &hcfg);
        let name = match harness.as_str() {
            HarnessId::CLAUDE => "claude",
            HarnessId::CODEX => "codex",
            HarnessId::OPENCODE => "opencode",
            HarnessId::GEMINI => "gemini",
            other => {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    format!("`{other}` can't answer one-off questions"),
                ));
            }
        };
        let binary = match (hcfg.path != "auto").then(|| PathBuf::from(&hcfg.path)) {
            Some(b) => b,
            None => loft_config::which(name, env.get("PATH").map(String::as_str).unwrap_or(""))
                .ok_or_else(|| {
                    ErrorBody::new(ErrorCode::Harness, format!("`{name}` isn't installed"))
                })?,
        };
        let kind = harness.clone();
        let run = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            use anyhow::Context as _;
            // An empty folder of the user's own: the CLI reads its project
            // settings (MCP servers, plugins, instructions) from where it
            // runs, and on Linux the temporary folder is shared with every
            // account on the computer.
            let dir = loft_config::private_temp_dir("agentloft-ask")
                .context("a private folder to ask in")?;
            let answer_file = dir.join(format!("agentloft-ask-{}.txt", uuid::Uuid::new_v4()));
            let mut cmd = std::process::Command::new(&binary);
            match kind.as_str() {
                HarnessId::CLAUDE => {
                    cmd.args(["-p", "--output-format", "text"]);
                    if let Some(m) = &model {
                        cmd.args(["--model", m]);
                    }
                    // A user default of high effort would take minutes here.
                    // No session: these don't belong in `claude --resume`.
                    cmd.args(["--effort", "low"]).args([
                        "--no-session-persistence",
                        "--tools",
                        "",
                        "--settings",
                        r#"{"disableAllHooks":true}"#,
                    ]);
                }
                HarnessId::CODEX => {
                    cmd.args(["exec", "--skip-git-repo-check", "--ephemeral"])
                        .args(["-s", "read-only", "-c", "model_reasoning_effort=low"])
                        // No tools wanted: leave out what would load them
                        // (and the user's hooks, which would run for it).
                        .args(CODEX_ASK_OFF.iter().flat_map(|key| ["-c", key]))
                        .arg("-o")
                        .arg(&answer_file);
                    if let Some(m) = &model {
                        cmd.args(["-m", m]);
                    }
                    // `-`: the prompt is on stdin.
                    cmd.arg("-");
                }
                HarnessId::OPENCODE => {
                    // No message: `opencode run` reads it from stdin.
                    cmd.arg("run");
                    if let Some(m) = &model {
                        cmd.args(["-m", m]);
                    }
                }
                _ => {
                    // No `-p`: Gemini runs headless when stdin isn't a
                    // terminal, and its prompt is what stdin says.
                    if let Some(m) = &model {
                        cmd.args(["-m", m]);
                    }
                }
            }
            use std::io::Write as _;
            use std::process::Stdio;
            let mut child = cmd
                .current_dir(&dir)
                .env_clear()
                .envs(env.iter())
                .env("NO_COLOR", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("run {}", binary.display()))?;
            let mut stdin = child.stdin.take().context("the agent's stdin")?;
            // Written beside reading its output: a long prompt fills the
            // pipe before the CLI has read it all. A CLI that stops reading
            // early (it failed) says why on stderr.
            let writer = std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
            let out = child
                .wait_with_output()
                .with_context(|| format!("run {}", binary.display()))?;
            let _ = writer.join();
            anyhow::ensure!(
                out.status.success(),
                "{} failed: {}",
                binary.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            let text = match std::fs::read_to_string(&answer_file) {
                Ok(text) => {
                    let _ = std::fs::remove_file(&answer_file);
                    text
                }
                Err(_) => String::from_utf8_lossy(&out.stdout).into_owned(),
            };
            Ok(strip_ansi(&text))
        });
        let text = tokio::time::timeout(std::time::Duration::from_secs(240), run)
            .await
            .map_err(|_| ErrorBody::new(ErrorCode::Harness, "The agent took too long"))?
            .map_err(internal)?
            .map_err(|e| ErrorBody::new(ErrorCode::Harness, format!("{e:#}")))?;
        Ok(Response::Text { text })
    }
}

/// `codex exec -c` overrides for a one-off answer: hooks, plugins, apps,
/// memories, subagents and web search off (checked against Codex 0.167;
/// unknown keys only draw a warning).
const CODEX_ASK_OFF: &[&str] = &[
    "features.hooks=false",
    "features.plugins=false",
    "features.apps=false",
    "features.memories=false",
    "features.multi_agent=false",
    "web_search=\"disabled\"",
];

/// Drop terminal colour codes from CLI output.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub(crate) fn harness_config(
    config: &loft_config::Config,
    harness: &HarnessId,
) -> loft_config::HarnessConfig {
    match harness.as_str() {
        HarnessId::CODEX => config.harness.codex.clone(),
        HarnessId::CLAUDE => config.harness.claude.clone(),
        HarnessId::OPENCODE => config.harness.opencode.clone(),
        HarnessId::GEMINI => config.harness.gemini.clone(),
        _ => loft_config::HarnessConfig::default(),
    }
}

/// Whether `icon` is already one Agentloft keeps (in `<home>/icons`), so it
/// isn't copied again. Resolved first: `icons/../x` is not in there.
fn already_an_icon(icon: &std::path::Path, home: &std::path::Path) -> bool {
    match (icon.canonicalize(), home.join("icons").canonicalize()) {
        (Ok(icon), Ok(dir)) => icon.parent() == Some(dir.as_path()),
        _ => false,
    }
}

/// The user's per-harness defaults for a new thread: model and mode.
pub(crate) fn thread_defaults(
    config: &loft_config::Config,
    harness: &HarnessId,
) -> (
    Option<String>,
    Option<loft_protocol::PermissionMode>,
    Option<String>,
) {
    let h = harness_config(config, harness);
    let set = |v: &str| (v != "auto" && !v.is_empty()).then(|| v.to_owned());
    let mode = serde_json::from_value(serde_json::json!(h.default_permission_mode)).ok();
    (set(&h.default_model), mode, set(&h.default_effort))
}

/// The login environment plus the agent's own `env` from its settings.
pub(crate) fn agent_env(
    base: &std::collections::HashMap<String, String>,
    hcfg: &loft_config::HarnessConfig,
) -> std::collections::HashMap<String, String> {
    let mut env = base.clone();
    env.extend(hcfg.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// What an agent's session runs with: [`agent_env`], plus the thread it
/// works for ([`THREAD_ENV`]) and, with the MCP bridge, the commands of
/// [`McpBridge::bin_dir`] first on its `PATH`.
pub(crate) fn session_env(
    base: &std::collections::HashMap<String, String>,
    hcfg: &loft_config::HarnessConfig,
    bridge: Option<&McpBridge>,
    thread: &ThreadId,
) -> std::collections::HashMap<String, String> {
    let mut env = agent_env(base, hcfg);
    env.insert(THREAD_ENV.into(), thread.to_string());
    if let Some(bridge) = bridge {
        // Windows' names aren't case-sensitive: one `PATH`, however spelt.
        let key = env
            .keys()
            .find(|k| k.eq_ignore_ascii_case("PATH"))
            .cloned()
            .unwrap_or_else(|| "PATH".into());
        let path = env.remove(&key).unwrap_or_default();
        let dir = bridge.bin_dir().to_string_lossy().into_owned();
        let path = if path.is_empty() {
            dir
        } else {
            format!("{dir}{}{path}", loft_config::PATH_SEPARATOR)
        };
        env.insert("PATH".into(), path);
    }
    env
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests;
