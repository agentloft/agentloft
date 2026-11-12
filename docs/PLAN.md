# Agentloft — Architecture & Delivery Plan

| | |
|---|---|
| **Status** | Draft v1. Research complete; no app code yet. |
| **Date** | 2026-11-08 |
| **Product name** | **Agentloft**. Working name was "Relay"; this repo folder is still `relay/`. |
| **Binaries** | `agentloft` (GPUI app), `agentloftd` (headless core daemon) |
| **Data home** | `~/.agentloft/` (override with `AGENTLOFT_HOME`) |
| **License** | Apache-2.0 ([ADR-0009](adr/0009-license-and-dependency-policy.md)) |
| **Platforms** | macOS (Apple Silicon) first. Every layer is designed to be cross-platform. |

Agentloft is a native desktop app, written in Rust with GPUI, that runs several coding-agent harnesses side by side: Claude Code, OpenAI Codex, and through ACP Gemini CLI, OpenCode and others. It is organised around projects, threads and per-thread git worktrees. It streams agent output, renders tool calls and diffs, handles approvals and permission modes, and includes an integrated terminal.

---

## Decision summary

| # | Decision | Status | ADR |
|---|---|---|---|
| 1 | Headless core runs as a **daemon (`agentloftd`) from M0**. The GPUI app is a client over a Unix socket. | Decided | [0001](adr/0001-daemon-from-m0.md) |
| 2 | **Structured protocols, never TUI scraping.** Native protocols for Claude and Codex; ACP for the long tail. | Decided | [0002](adr/0002-structured-protocols-not-tui-scraping.md) |
| 3 | Claude: the `claude -p` stream-json **control protocol, implemented in Rust**. No Node sidecar. | Decided | [0003](adr/0003-claude-stream-json-in-rust.md) |
| 4 | Codex: `codex app-server` JSON-RPC over stdio, with Rust types **generated from the schema of a pinned Codex version**. | Proposed | [0004](adr/0004-codex-app-server-generated-types.md) |
| 5 | GPUI through **gpui-kit's exact `gpui-pre` pin**, kept current with a regular bump cadence. | Proposed | [0005](adr/0005-gpui-pinning-and-staying-current.md) |
| 6 | **SQLite** (rusqlite, WAL): CRUD tables plus an append-only per-thread event log. **TOML** config. Everything lives under `~/.agentloft`. | Proposed | [0006](adr/0006-storage-sqlite-event-log-toml.md) |
| 7 | Terminal: PTYs are owned by the daemon. The UI renders them with **libghostty-vt** (`ghostty-vt`, Zig 0.17 build) and a custom GPUI element, with `alacritty_terminal` as the fallback backend. | Accepted (amended) | [0007](adr/0007-terminal-architecture.md) |
| 8 | Git through the **`git` CLI**. Per-thread worktrees under `~/.agentloft/worktrees`. Checkpoints stored as hidden refs. | Proposed | [0008](adr/0008-git-worktrees-checkpoints.md) |
| 9 | **Apache-2.0**. The dependency policy blocks GPL and AGPL; enforced by `cargo-deny`. | Decided | [0009](adr/0009-license-and-dependency-policy.md) |
| 10 | Tokio in the daemon, GPUI's executor in the UI, and a thin client bridge between them. | Proposed | [0010](adr/0010-async-runtimes.md) |
| 11 | A thread is **pinned to one provider**. **Forks can target any provider**: a native fork for the same provider, a context handoff otherwise. | Decided | [0011](adr/0011-threads-forks-provider-pinning.md) |
| 12 | **Multiple windows. Window groups are user-defined and not tied to a project**, so one tabbed group can hold agents from different projects. | Decided | [0012](adr/0012-multi-window-and-window-groups.md) |
| 13 | **Agents can read other threads** through a built-in, read-only **Agentloft MCP server** attached to every harness. | Decided | [0013](adr/0013-agentloft-mcp-cross-thread-context.md) |
| 14 | **Remote access** over the user's own **Tailscale** tailnet, or an SSH tunnel. Devices must be paired; nothing is exposed publicly. | Decided (design) | [0014](adr/0014-remote-access-tailscale.md) |
| — | Worktree default **`ask`**; branches named **`agentloft/<thread-slug>`**; cross-provider handoff uses a **deterministic digest**; merge-back offers **both local squash-merge and a `gh` PR**. | Decided | §2.8, §5.2 |
| — | Permission mode: **the harness's own default** unless the user sets a per-harness default in settings, or a per-thread override. | Decided | §2.7 |
| — | Telemetry: **none**. A `loft-telemetry` no-op trait is the seam for opt-in telemetry later. | Decided | §8 |
| — | Importing existing Claude and Codex history: **not now**. | Decided | §8 |
| — | Signing and notarization: **deferred**. Distribute as a direct download. | Decided | §6 |

---

## 0. Product scope

**Goals for v1**
- A fast, native, keyboard-first app that drives Claude Code and Codex with full fidelity: streaming, approvals, interrupts, resume, model and mode switching, forks.
- Projects contain threads. Each thread runs in the repo or in its own git worktree. Every turn is checkpointed, so turn and cumulative diffs are always available and revertible.
- Agents keep running when the UI is closed or crashes (the daemon owns them).
- Multiple windows, which can be grouped as native macOS window tabs.
- An integrated terminal per thread or project.

**Non-goals for v1**
- Agentloft never handles model API keys. Agents use their own CLI login (`claude login`, `codex login`, and so on).
- No TUI scraping or keystroke injection ([ADR-0002](adr/0002-structured-protocols-not-tui-scraping.md)).
- No embedded code editor or browser, and no mobile client in v1. Remote desktop-to-desktop access over Tailscale or SSH is planned for M8 (ADR-0014), and the protocol keeps the door open for mobile.
- No telemetry and no history import.
- No Linux or Windows builds until after the macOS v1. The code must still compile cleanly for those targets from M0.

---

## 1. Architecture

### 1.1 Process model

```
┌──────────────── Agentloft.app ─────────────────┐
│ agentloft (GPUI, main thread + GPUI executor)  │   N windows, native tab groups
│   loft-ui views ── loft-client (mirror store)  │
└───────────────┬────────────────────────────────┘
                │ Unix socket ~/.agentloft/run/agentloftd.sock (0600)
                │ length-delimited JSON frames: hello / request / subscribe / event
┌───────────────▼────────────────────────────────┐
│ agentloftd (tokio)                             │
│  loft-core: decider · orchestrator · effects   │
│  loft-store (SQLite WAL)  loft-git  PTY host   │
│  harness supervisor ─┬─ claude -p (stream-json, 1 proc / active thread)
│                      ├─ codex app-server (JSON-RPC, 1 proc / Codex instance)
│                      └─ ACP agents (gemini --acp, opencode acp, …)
│        ▲ each agent gets MCP server "agentloft" = `agentloftd mcp --thread <id>`
│          (stdio bridge → daemon socket; read-only thread tools, ADR-0013)
│  optional remote listener (off by default): WebSocket on 127.0.0.1 behind
│  `tailscale serve`, or tailnet IP; paired devices only (ADR-0014)
└────────────────────────────────────────────────┘
          ▲ remote Agentloft app / future mobile client over the user's tailnet or SSH
```

- **`agentloftd`** owns all durable state and every external process: agent CLIs, PTYs and git. It has no GPUI dependency and runs fine headless (tests, CI, a future CLI or remote clients).
- **`agentloft`** is a thin client. It mirrors the projections it needs, sends commands, and renders.
  - One UI process serves all windows over **one** socket connection, which multiplexes subscriptions.
  - If the UI crashes, no work is lost.
- **Daemon lifecycle:**
  - **M0–M6:** the UI starts the daemon on demand, detached with `setsid`, if the socket is dead.
  - **M7:** the daemon is registered as a LaunchAgent through `SMAppService` (macOS 13+), using the plist in `Contents/Library/LaunchAgents`, with `KeepAlive` set.
  - A lock file (`run/agentloftd.lock`, held with `flock`) guarantees a single instance per `AGENTLOFT_HOME`.

### 1.2 Why a daemon from M0, and how it is updated

**Why** (detail in [ADR-0001](adr/0001-daemon-from-m0.md)):
1. Agent turns run for tens of minutes. Quitting, updating or crashing the UI must not kill them. UI code changes far more often than core code.
2. It enforces the headless-core boundary from day one, so there is never a GPUI dependency leaking into the domain.
3. Remote, mobile and CLI clients come later for almost nothing.

The cost is an IPC protocol, version negotiation, and two processes to debug. We accept it.

**Updates:**

| Situation | Behaviour |
|---|---|
| UI-only change (most releases) | The UI restarts. The daemon and its agents are untouched, and the UI reconnects and catches up from the event log. |
| Daemon change, daemon **idle** (no running turn, no pending approval) | 1. The new UI connects and sees `daemon_build != bundled_build`. 2. It sends `Shutdown{reason: Upgrade}`. 3. The daemon takes an online DB backup (`VACUUM INTO`), runs no migrations, closes harness processes gracefully and exits. 4. The UI (later launchd) starts the new daemon. 5. The new daemon migrates and resumes sessions lazily from their native IDs. Takes about 1–2 s. |
| Daemon change, daemon **busy** | The UI shows a banner: "Core update ready — applies when agents are idle" plus **[Restart now]**. The daemon sets `upgrade_pending` and restarts itself at the next fully idle moment. |
| Forced restart | In-flight turns are marked `interrupted`. The conversation is safe: the next message resumes the native session (`claude --resume`, Codex `thread/resume`). Terminal shells restart, with a warning if a foreground job was running. |
| Protocol compatibility | Integer `protocol_version` in the hello handshake. The daemon supports N and N−1. The UI refuses to talk to an older-incompatible daemon and asks it to upgrade. DB migrations are forward-only. |

The later, optional upgrade is per-agent **session holders** (M9): a tiny `agentloft-hold` process that owns each agent's stdio and a replay buffer, so turns survive even a daemon restart. This is the same idea as Ghostex's `zmx`, but for structured stdio. Defer it until long unattended runs prove the need.

### 1.3 Workspace layout

A single Cargo workspace. All crates are `publish = false`; the short internal prefix is `loft-`.

```
Cargo.toml                 # [workspace], shared deps, profiles (opt-level=1 for dev deps, lto=thin release)
rust-toolchain.toml        # pinned (1.99.x today)
deny.toml                  # license allowlist (no GPL/AGPL), advisories, bans
.cargo/config.toml         # aliases, linker settings
xtask/                     # cargo xtask dev | test-daemon | schema-codex | bundle | fixtures
crates/
  loft-app/                # bin `agentloft`: GPUI bootstrap, menus, window/tab-group mgmt, keymap
  loft-ui/                 # views: sidebar, timeline, items, composer, diff, terminal element, settings, theme
  loft-client/             # GPUI-free socket client: handshake, reconnect, subscriptions, mirrored projections
  loft-protocol/           # wire types: Hello, Command, Query, Event, Snapshot, ids, versioning (serde)
  loftd/                   # bin `agentloftd`: socket server, lifecycle, upgrade/shutdown, single-instance lock
  loft-core/               # domain model, pure decider, orchestrator, effect runner, session supervisor
  loft-store/              # rusqlite, migrations, event log, projections, backups
  loft-harness/            # Harness/HarnessSession traits, canonical events, capabilities,
                           # stdio JSONL + JSON-RPC framing, process-group supervisor, env capture
  loft-harness-claude/     # stream-json + control protocol
  loft-harness-codex/      # app-server client; schema/<codex-version>/*.json → generated types
  loft-harness-acp/        # agent-client-protocol client (Gemini, OpenCode, Cursor, Copilot, …)
  loft-git/                # git CLI wrapper: status, worktrees, checkpoints, diffs
  loft-mcp/                # Agentloft MCP server (`agentloftd mcp`): read-only thread tools, ADR-0013 (official Rust MCP SDK `rmcp`)
  loft-remote/             # WebSocket listener, Tailscale probe/serve config, device pairing (M8, ADR-0014)
  loft-pty/                # daemon-side PTY host (portable-pty), ring buffers
  loft-config/             # TOML config, paths (~/.agentloft), per-project config
  loft-testkit/            # FakeHarness, fixture recorder/replayer, temp AGENTLOFT_HOME, git repo builders
docs/  PLAN.md  adr/
```

Rules:
- `loft-core`, `loft-store`, `loft-harness*`, `loft-git`, `loft-pty` and `loftd` must **never** depend on `gpui`.
- `loft-ui` and `loft-app` never spawn processes or touch SQLite.
- `loft-protocol` is the only crate shared by both sides.

### 1.4 Domain model

```
Project            id, name, root_path, default_harness, settings (worktree mode, setup script)
 └─ Thread         id, project_id, title, harness_id (pinned), model, effort, permission_mode (Option = harness default),
                   workspace {Repo | Worktree{path, branch, base}}, native_session_id, parent {thread_id, turn_id, kind}?,
                   status {idle | running | waiting_approval | error | archived}
     └─ Turn       id, ordinal, status {running | completed | interrupted | failed}, started/ended, usage, checkpoint_before/after
         └─ Item   id, kind, status, payload (kind-specific), native ids
RuntimeRequest     id, thread/turn/item, kind {tool_approval | file_change_approval | permissions | user_question | elicitation},
                   options offered, status {open | resolved | expired}, decision
Checkpoint         thread_id, ordinal, git ref, commit sha, turn_id
TerminalSession    id, scope {project | thread}, cwd, shell, status
```

**Item kinds** are the union of what Claude, Codex and ACP emit:
`user_message`, `assistant_message`, `reasoning`, `command_execution`, `file_change`, `file_read`, `search` (file or web), `mcp_tool_call`, `other_tool_call`, `plan`/`todo_list`, `subagent`, `approval_request`, `user_input_request`, `notice` (system, info or warning), `error`, `compaction`, `checkpoint`, `fork_marker`, `handoff`.

### 1.5 Command → event flow

The orchestration model, simplified:

1. A client sends `Command{command_id, body}`, for example `SendMessage{thread_id, text, attachments}`.
2. `Orchestrator` (one actor per thread, plus a workspace actor) runs a **pure decider**: `decide(state, command) -> Vec<DomainEvent>`. It has no I/O and is fully unit-testable.
3. `loft-store` appends the events and updates the projections **in one SQLite transaction**. It also stores a command receipt; a duplicate `command_id` returns the same result.
4. The **effect runner** reacts to committed events. It starts a turn on the harness session, captures a checkpoint, or creates a worktree. Results come back as internal commands, for example `HarnessEventObserved{…}`.
5. Committed events are fanned out to subscribers. Ephemeral **deltas** (text and output chunks) go to subscribers immediately and are coalesced into item snapshots (§4.4).

### 1.6 Wire protocol (UI ↔ daemon)

- **Transport:** Unix domain socket in a `0700` directory, socket mode `0600`. Peer UID is checked with `getpeereid`.
  - Remote access ([ADR-0014](adr/0014-remote-access-tailscale.md), §5.7) reuses **the same frames** over a WebSocket, adding device authentication.
  - The protocol is transport-agnostic from M0. `Hello` carries `client_id`, a `client_kind` (`local-ui | remote-ui | mcp-bridge | cli`) and an optional auth proof, so remote access needs no protocol redesign.
- **Framing:** a `u32` big-endian length prefix with a JSON body (`tokio-util` `LengthDelimitedCodec` plus `serde_json`). JSON keeps it debuggable. A binary codec can be negotiated later through `hello` if profiling shows a need.
- **Messages:**
  - `Hello{protocol_version, client_build}` → `Welcome{daemon_build, protocol_version, features}`
  - `Request{id, Command|Query}` → `Response{id, Ok|Err}`
  - `Subscribe{topic: Workspace | Thread(id) | Terminal(id), after_seq?}` → `Snapshot{…, seq}`, then a stream of `Event{seq, …}`
  - Ephemeral `Delta{thread_id, item_id, chunk}` and `TerminalOutput{id, bytes}`
- **Reconnect:** resubscribe with `after_seq`. The daemon replays from the event log, or sends a fresh snapshot if the gap is large.

### 1.7 Process environment

macOS GUI apps do not inherit the shell `PATH`, so `claude`, `codex` and `node` are often not found. The daemon captures the user's login-shell environment **once at startup**. It runs `$SHELL -l -i -c 'env -0'` with a timeout and falls back to the existing environment plus well-known paths (`/opt/homebrew/bin`, `~/.local/bin`, `~/.bun/bin`, the nvm/volta shims). Binary paths can be overridden per harness in `config.toml`. Zed had to solve this too.

---

## 2. Harness abstraction

### 2.1 Traits

```rust
#[async_trait]
pub trait Harness: Send + Sync + 'static {
    fn id(&self) -> HarnessId;                          // "claude", "codex", "acp:gemini", …
    fn capabilities(&self) -> &Capabilities;
    async fn probe(&self) -> ProbeReport;               // installed? path, version, auth state, known-good range
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;   // id, label, efforts, default
    async fn list_commands(&self, cwd: &Path) -> Result<Vec<SlashCommand>>;
    async fn open_session(&self, cfg: SessionConfig) -> Result<Box<dyn HarnessSession>>;
    // SessionConfig { cwd, model, effort, permission_mode: Option<PermissionMode>,
    //                 resume: Option<NativeSessionId>, fork_from: Option<NativeFork>, env, extra_args }
}

#[async_trait]
pub trait HarnessSession: Send {
    fn events(&mut self) -> BoxStream<'static, HarnessEvent>;
    fn native_session_id(&self) -> Option<NativeSessionId>;
    async fn start_turn(&mut self, input: TurnInput) -> Result<NativeTurnId>;  // text + images + file refs
    async fn steer(&mut self, input: TurnInput) -> Result<()>;                  // Capability::Steer
    async fn interrupt(&mut self) -> Result<()>;
    async fn respond(&mut self, req: NativeRequestId, resp: RequestResponse) -> Result<()>;
    async fn set_model(&mut self, model: &str, effort: Option<Effort>) -> Result<()>;
    async fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<()>;
    async fn revert(&mut self, to: RevertTarget) -> Result<()>;                 // Capability::NativeRevert
    async fn shutdown(self: Box<Self>, grace: Duration) -> Result<()>;
}
```

Optional methods return `Err(Unsupported)`. The core always checks `Capabilities` first and never branches on provider name.

### 2.2 Canonical events

```
SessionReady { native_session_id, model, commands, models? }
TurnStarted  { native_turn_id }
ItemStarted  { item_id, kind, payload }        ItemDelta { item_id, delta: Text | Reasoning | Output | Patch }
ItemCompleted{ item_id, payload, status }
RequestOpened{ request_id, kind, item_id?, options: Vec<DecisionOption>, payload }
RequestResolved{ request_id, by: User | Harness | Timeout }
PlanUpdated  { steps }                          Usage { input, output, cached, cost_usd? }
TurnCompleted{ status: Completed | Interrupted | Failed{error}, usage }
Notice{level, text}   Error{fatal, text}   SessionExited{code, signal}
```

### 2.3 Capabilities

| Flag | Claude | Codex | ACP (typical) |
|---|---|---|---|
| streaming text deltas | ✅ `stream_event` | ✅ `item/agentMessage/delta` | ✅ `agent_message_chunk` |
| reasoning stream | ✅ thinking blocks | ✅ `item/reasoning/*Delta` | ✅ `agent_thought_chunk` |
| steer mid-turn | ✅ (stdin user msg while running; queued by CLI) | ✅ `turn/steer` | ❌ (extension only) |
| interrupt | ✅ control `interrupt` | ✅ `turn/interrupt` | ✅ `session/cancel` |
| live model switch | ✅ control `set_model` | ✅ per-turn `model`/`effort` | ⚠️ `session/set_config_option` if offered |
| live mode switch | ✅ control `set_permission_mode` | ✅ per-turn `approvalPolicy`/`sandboxPolicy` | ⚠️ `session/set_mode` |
| approval "always/session" | ✅ `updatedPermissions` | ✅ `acceptForSession`, execpolicy amendment | ✅ `allow_always` |
| native fork | ✅ `--resume … --fork-session` (+ `--resume-session-at`) | ✅ `thread/fork{lastTurnId, cwd}` | ⚠️ unstable `session/fork` |
| native revert | ✅ control `rewind_files` (files only) | ✅ `thread/revert{beforeTurnId}` | ❌ |
| model listing | ✅ `initialize` control response `models` (verified 2.1.319) | ✅ `model/list` | ⚠️ config options |
| slash commands | ✅ from init / initialize | ✅ `skills/list` | ✅ `available_commands_update` |
| turn diff from agent | ❌ (use checkpoints) | ✅ `turn/diff/updated` (we use checkpoints anyway) | ❌ |

### 2.4 Claude Code ([ADR-0003](adr/0003-claude-stream-json-in-rust.md))

**Spawn.** One process per *active* thread. It stays alive across turns because stream-json input keeps stdin open. It is closed after an idle timeout (default 15 min) and reopened with `--resume`.

```
claude -p --output-format stream-json --input-format stream-json --verbose \
  --include-partial-messages --replay-user-messages \
  --permission-prompt-tool stdio \
  (--session-id <uuid we pre-assign> | --resume <id> [--fork-session] [--resume-session-at <msg-uuid>]) \
  [--model <m>] [--permission-mode <mode>  # only if user set one; else harness default] \
  [--append-system-prompt …] [--mcp-config …]
```

Pre-assigning `--session-id` means the native ID is known before the first byte arrives.

**Wire.** NDJSON in both directions.
- **stdout:**
  - `system/init`: session_id, model, tools, slash commands, MCP status.
  - `stream_event`: partial deltas.
  - `assistant` / `user`: full messages, with `parent_tool_use_id` for subagents.
  - `result`: the turn ends, with cost, usage and denials.
  - `control_request` from the CLI: `can_use_tool`, `hook_callback`, `mcp_message`.
  - `control_response`.
- **stdin:**
  - `user` messages: text plus image content blocks.
  - `control_request` from us: `initialize`, `interrupt`, `set_permission_mode`, `set_model`, `rewind_files`, `get_context_usage`, `mcp_status`.
  - `control_response` replies to `can_use_tool`: `{behavior:"allow", updatedInput, updatedPermissions?}` or `{behavior:"deny", message, interrupt?}`.

**Mapping.**
- `tool_use` blocks become items. The kind comes from the tool name: Bash → command, Edit/Write/MultiEdit → file_change, Read → file_read, Grep/Glob → search, WebSearch/WebFetch → search, `mcp__*` → mcp_tool_call, Task/Agent → subagent, TodoWrite → todo_list.
- The matching `tool_result` completes the item.

**Approvals.** `can_use_tool` becomes `RequestOpened` with these options:
- Allow once
- Allow for session: `updatedPermissions` built from the CLI's `permission_suggestions`
- Deny
- Deny and stop: `interrupt: true`

**Types.** Our own serde models in `loft-harness-claude`. They are *tolerant*: `#[serde(other)]` and unknown-field passthrough into `Item::Unknown{raw}`, which renders as a generic row, never a crash. Evaluate the community crate `claude-codes` (it tracks CLI versions) as a reference or test oracle. Check its license before depending on it.

**Risk.** The control protocol has no public spec; it is defined by the official SDK sources (Python and TS, MIT). Mitigations:
- Record fixtures per CLI version.
- A known-good version range shown in onboarding.
- A nightly canary against the latest CLI (§2.10).
- Fallback design: a `loft-harness-claude-sdk` sidecar that runs the official TS Agent SDK under Bun behind the same trait. We would only build it if the CLI protocol becomes unworkable.

### 2.5 Codex ([ADR-0004](adr/0004-codex-app-server-generated-types.md))

**Spawn.** `codex app-server` (stdio, JSONL JSON-RPC with `jsonrpc` omitted).
- One process per Codex instance, keyed by `CODEX_HOME`, serving many threads. Respawn on crash, then `thread/resume` for active threads.
- **Never** attach to the managed `codex app-server daemon`. Ghostex saw shared-daemon lock and environment bleed problems.
- A config flag allows one process per thread if per-cwd environment bleed shows up.

**Handshake.** `initialize{clientInfo, capabilities:{experimentalApi:true, optOutNotificationMethods:[…]}}`, then the `initialized` notification.

**Calls:**
- `thread/start{cwd, model, approvalPolicy?, sandbox?}` or `thread/resume{threadId}` or `thread/fork{threadId, lastTurnId, cwd}`
- `turn/start{threadId, input[], model?, effort?, approvalPolicy?, sandboxPolicy?}`, `turn/steer{expectedTurnId}`, `turn/interrupt`
- `thread/revert{threadId, beforeTurnId}`, `model/list`, `skills/list`, `thread/compact/start`

**Events:**
- `turn/started` and `turn/completed`. `turn/completed` is authoritative: `turn/interrupt` returns first and the interrupted status arrives later.
- `item/started` and `item/completed`, with item types agentMessage, reasoning, commandExecution, fileChange, mcpToolCall, webSearch, plan…
- Deltas: `item/agentMessage/delta`, `item/reasoning/textDelta|summaryTextDelta`, `item/commandExecution/outputDelta`, `item/fileChange/outputDelta`.
- `turn/plan/updated`, `thread/tokenUsage/updated`, `serverRequest/resolved`, `error`, `warning`, `model/rerouted`.

**Approvals** (server→client requests):
- `item/commandExecution/requestApproval`: decision is `accept`, `acceptForSession`, `acceptWithExecpolicyAmendment{…}`, `applyNetworkPolicyAmendment{…}`, `decline` or `cancel`.
- `item/fileChange/requestApproval`: `accept`, `acceptForSession`, `decline` or `cancel`.
- `item/permissions/requestApproval`: `{permissions, scope: turn|session}`.
- `item/tool/requestUserInput` → `{answers}`, and `mcpServer/elicitation/request`.
- The legacy `execCommandApproval` and `applyPatchApproval` are handled for older CLIs.

**Types.** `cargo xtask schema-codex` runs `codex app-server generate-json-schema --out crates/loft-harness-codex/schema/<version>`. `typify` then generates serde types into checked-in Rust, which avoids build-time codegen surprises. Do **not** depend on OpenAI's `codex-app-server-protocol` crate: it is unpublished, git-only, and pulls in a large dependency tree. Verified locally against codex-cli 0.167.0: the method lists above come from its generated schema.

### 2.6 ACP harnesses: Gemini, OpenCode and the long tail

- **Crate:** `agent-client-protocol` 5.x (Apache-2.0), speaking the stable v1 schema. v2 is alpha; track it, don't adopt it yet.
- **Calls:** `initialize`, `session/new{cwd, mcpServers}` / `session/load` / `session/resume`, `session/prompt`, `session/cancel`, `session/set_mode`, `session/set_config_option`.
- **Updates:** `session/update` carries `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update` and `usage_update`.
- **Client-side methods:** `session/request_permission` (`allow_once`, `allow_always`, `reject_once`, `reject_always`). We implement `fs/read_text_file` and `fs/write_text_file` against the thread's cwd and `terminal/*` against `loft-pty`. Agents that prefer their own shells can still use them.
- **Registry config:** a simple `[[harness.acp]]` list in `config.toml` with name, command and args. Built-in presets:

| Agent | Command |
|---|---|
| Gemini CLI | `gemini --acp` |
| OpenCode | `opencode acp` |
| Cursor | `cursor-agent acp` |
| Copilot CLI | `copilot --acp` |
| Goose | `goose acp` |
| Qwen Code | `qwen --acp` |

  Verify each flag when adding its preset.
- **What ACP loses** compared with native protocols:
  - Codex's execpolicy and network amendments
  - steering
  - rich revert and fork
  - granular item types
  - rate-limit and account state

  That is why Claude and Codex stay native.
- **Later native options:** Gemini headless `-p --output-format stream-json` has no interactive approvals, so ACP is better. OpenCode `serve` (HTTP + SSE) is richer than ACP and is a candidate for a native adapter after M6.

### 2.7 Cross-provider mapping

**Permission modes.** The default is **the harness's own default**: Agentloft passes no mode flag, so `~/.claude/settings.json` and `~/.codex/config.toml` apply. Settings let the user set a per-harness default, and each thread can override it from the composer footer. Agentloft's canonical modes:

| Agentloft mode | Claude `--permission-mode` | Codex `approvalPolicy` + `sandbox` | ACP |
|---|---|---|---|
| *Harness default* | *(omit flag)* | *(omit both)* | *(agent default)* |
| Read-only / plan | `plan` | `untrusted` + `read-only` | `set_mode("plan")` if offered |
| Ask for everything | `default` (`manual`) | `untrusted` + `workspace-write` | agent's ask mode |
| Auto-accept edits | `acceptEdits` | `on-request` + `workspace-write` | agent's edit mode |
| Auto (agent judges) | `auto` | `on-request` + `workspace-write` + auto-review | — |
| Full access | `bypassPermissions` | `never` + `danger-full-access` | agent's yolo mode |

**Other operations:**

| Operation | Claude | Codex | ACP |
|---|---|---|---|
| Approve once | `allow` | `accept` | `allow_once` |
| Approve for session | `allow` + `updatedPermissions` | `acceptForSession` / execpolicy amendment | `allow_always` |
| Deny, continue | `deny` | `decline` | `reject_once` |
| Deny and stop turn | `deny` + `interrupt:true` | `cancel` | `reject_once` + `session/cancel` |
| Interrupt | control `interrupt` | `turn/interrupt` (wait for `turn/completed`) | `session/cancel` |
| Session id | pre-assigned `--session-id` UUID | `thread/start` → `threadId` | `session/new` → `sessionId` |
| Resume after restart | `--resume <id>` | `thread/resume{threadId}` | `session/load` / `session/resume` |
| Model | `--model`, control `set_model` | `model/list`; per-turn `model`, `effort` | config option / mode |
| Slash commands | init `slash_commands`, sent as text | `skills/list`; `$skill` input items | `available_commands_update` |
| Images | base64 image content blocks | `localImage{path}` input | `image` content block if the `promptCapabilities.image` capability is set |

### 2.8 Forks ([ADR-0011](adr/0011-threads-forks-provider-pinning.md))

A thread is pinned to its provider. **Fork** creates a new thread from any turn of the parent:
- **Same provider:** a native fork.
  - Claude: `--resume <id> --fork-session [--resume-session-at <msg>]`.
  - Codex: `thread/fork{threadId, lastTurnId, cwd}`.
- **Different provider:** a **context handoff**. The new thread starts with a **deterministic digest** generated from the event log. It is instant, free and reproducible, and contains the user goals, the assistant's final answers, the files changed, the current diff summary and open TODOs. It is trimmed to a token budget, and the user can review and edit it.
  - The digest ends with a pointer: *"Full history: thread `<id>`. Use the `agentloft` MCP tools to read it."*
  - Through the MCP tools (§2.9), the new agent can pull exact details from the parent on demand. This replaces the "ask the source agent to summarise" option.
- **Workspace:** by default the fork gets **a new worktree branched from the parent's checkpoint commit at that turn**, so the forked code state matches the conversation state. Option: share the parent's workspace.
- **Lineage:** stored as `parent{thread_id, turn_id, kind: native|handoff}`. The sidebar shows forks nested under their parent.

### 2.9 Cross-thread context: the Agentloft MCP server ([ADR-0013](adr/0013-agentloft-mcp-cross-thread-context.md))

Agents can **read other threads**, for example "look at what the Codex thread found about the auth bug". Every harness speaks MCP, so the daemon exposes a built-in MCP server.

- **Process:** `agentloftd mcp --thread <id>` is a stdio MCP server. It is the same binary in a different mode, and it bridges to the daemon socket as `client_kind = mcp-bridge`, carrying a per-session capability token.
- **Attaching it to each harness:**
  - Claude: a generated `--mcp-config`.
  - Codex: `-c mcp_servers.agentloft.command=… -c mcp_servers.agentloft.args=[…]` on the app-server spawn, or the per-thread `config` in `thread/start`.
  - ACP: `session/new{mcpServers}`.
- **Tools** (all read-only in v1):

| Tool | Returns |
|---|---|
| `list_threads{project?, query?, limit}` | id, title, harness, status, updated, parent/fork lineage |
| `read_thread{thread_id, from_turn?, to_turn?, detail: summary\|messages\|full}` | Transcript slices: user messages, final answers, tool calls (summarised by default), approvals |
| `search_threads{query, project?}` | FTS5 hits with thread, turn and snippet |
| `get_thread_diff{thread_id, turn?\|cumulative}` | Unified diff from checkpoints |
| `get_thread_plan{thread_id}` | Latest plan and todo items |

- **Scope and permissions:**
  - Default: threads in the **same project**, plus the thread's own parent and children.
  - Setting `context.cross_thread_scope = project | all | off`. Per-thread toggle in the composer footer.
  - Archived threads are excluded unless asked for. Responses are size-capped and paginated.
  - Tool calls appear in the timeline like any other `mcp_tool_call`, so the user always sees what was read.
- **Later (not v1):** write tools such as `post_to_thread` and `spawn_thread` for agent-to-agent orchestration. They would sit behind approvals and their own ADR.

### 2.10 Staying current with CLI and GPUI changes

Yes, this is designed in:

- **Compatibility gates.** Each harness declares a tested version range. `probe()` warns, but does not block, outside that range.
- **Contract fixtures.** `xtask fixtures record` runs scripted scenarios against the installed CLI (text turn, tool approval, deny, interrupt, resume, fork, image) and stores the NDJSON transcripts under `crates/loft-harness-*/fixtures/<cli-version>/`. CI replays every fixture set through the adapters and asserts the canonical event sequences.
- **Nightly canary** (local machine or a self-hosted runner with CLI logins):
  1. Install the latest `claude` and `codex`.
  2. Run the live scenarios.
  3. Regenerate the Codex schema and **diff it** against the committed one.
  4. Open an issue or PR on drift.
- **Tolerant parsing everywhere.** Unknown message types become `Unknown{raw}` items and are logged, not fatal.
- **GPUI:** see [ADR-0005](adr/0005-gpui-pinning-and-staying-current.md). Bump gpui-kit and `gpui-pre` together every 2–4 weeks in a dedicated PR, with a UI smoke test and screenshot review.

---

## 3. UI

### 3.1 Windows and window groups ([ADR-0012](adr/0012-multi-window-and-window-groups.md))

- **A window (tab) shows one thread**, an "agent chat", from **any project**. The sidebar lists all projects. Opening a thread puts it in the current group, or in a new window with ⌘⇧O.
- **Window groups are user-defined and not tied to projects.** A group is a named, tabbed window, for example "Auth refactor" holding a Claude thread from `api/`, a Codex thread from `web/` and a Gemini thread from `infra/`.
  - Groups are first-class entities (`window_groups` table): name, ordered tabs (thread ids), active tab, frame.
  - Actions: new group, rename, move tab to group (also by dragging a tab between windows), close group (threads keep running), reopen group.
  - Default on first launch: one group named "Main".
- **Implementation:** native macOS window tabs. GPUI exposes `WindowOptions.tabbing_identifier` and `SystemWindowTabController` (verified in Zed `main`). Each group has a unique identifier (`group:<uuid>`), so all its windows merge into one native tabbed window. On Linux and Windows there is an in-app tab strip with the same model.
- Groups, tab order, window frames and per-tab UI state (scroll position, dock layout) are persisted in the daemon's `ui_state` and `window_groups` tables and restored on launch. Remote clients get their own `client_key`.
- All windows share one `loft-client` store. GPUI `Entity`s per thread are shared, so two windows showing the same thread render the same live state for free.

### 3.2 Layout

```
┌ sidebar ─────────┬ thread ─────────────────────────────────────┬ right dock (toggle) ┐
│ ▸ Project A      │ header: title · harness · model · mode · ⎇  │ Diff | Terminal |   │
│   ● thread 1     │                                             │ Plan | Files        │
│   ◐ thread 2 ⤷   │ timeline (virtualized, tail-follow)         │                     │
│ ▸ Project B      │                                             │                     │
│                  │ ───────────────────────────────────────────  │                     │
│ [+ project]      │ composer  [attachments] [/cmd] [@file]      │                     │
│ needs-attention  │ footer: model ▾ effort ▾ mode ▾  ⏹ / ⏎     │                     │
└──────────────────┴─────────────────────────────────────────────┴─────────────────────┘
```

Layout uses gpui-kit `Resizable` and `Dock`. The right dock can be split. The bottom terminal panel is an alternative placement.

### 3.3 Components

| UI piece | Source | Notes |
|---|---|---|
| Sidebar tree (projects → threads → forks) | gpui-kit `Sidebar` + `Tree`/`List` | Status dots: running, waiting for approval, error, unread. Context menu: rename, fork, archive, reveal worktree. |
| Timeline | gpui-kit **`MessageScroller`** (virtualized, variable height, follows the tail, `remeasure_items`) | One row per Item. Collapsed tool cards. Sticky "jump to latest". |
| Assistant markdown | gpui-kit **`TextView`** (markdown-rs, `push_str` streaming, selection, code-block actions) | See §3.4 for the streaming strategy. |
| Code blocks | `TextView` code blocks + tree-sitter highlighting | Copy and "open in editor" actions. |
| Command card | Custom | Command line, cwd, exit code, duration, streaming output (ANSI → styled runs, capped and expandable). |
| File-change card | gpui-kit **`Diff`** (unified/split, word-level, virtualized) | Inline per item. A "view in dock" action opens the full diff. |
| Approval card | Custom + `Button` group | Options come from `RequestOpened.options`. Keyboard: `y` once, `a` always, `n` deny, `esc` deny+stop. A count also appears in the sidebar's needs-attention badge. |
| Composer | gpui-kit **`Input` (multi-line) / `Editor`** with IME, undo, paste hooks | Slash-command popover, `@file` fuzzy mentions, image paste/drop. Messages queue while a turn runs, with "steer now" for harnesses that support it; the daemon holds the queue and sends it (TRACKING ▸ Long-running threads). Per-thread draft persisted. |
| Diff dock | gpui-kit `Diff` | Turn diff, cumulative diff, and comparison against any checkpoint. File list. Revert actions. |
| Terminal | Custom GPUI element on libghostty-vt (`loft-terminal`, [ADR-0007](adr/0007-terminal-architecture.md)) | Tabs per thread; ⌘-click URLs. |
| Command palette | gpui-kit `Command` | All GPUI actions. |
| Settings | gpui-kit `Settings`/forms; edits `config.toml` through the daemon | Harnesses (path, version, auth status, default model and mode), worktrees, appearance, keymap. |
| Notifications | gpui-kit `Notification` in-app + macOS user notifications | Turn finished or needs approval while unfocused. |

### 3.4 Streaming markdown strategy

- Deltas arrive from the daemon about every 10–50 ms. The UI coalesces them **per frame**. A notification marks the item entity dirty, and at most one re-render happens per frame.
- To avoid O(n²) re-parsing on long messages, split the assistant text into **stable blocks plus a live tail**. When a block boundary closes (a blank line outside a code fence), freeze the preceding blocks into an immutable `TextView`. Only the tail block is re-parsed on each delta.
- Code fences in the tail render as plain monospace until closed, then get highlighted.
- Budget: 120 fps target, never below 60 fps, on a 10k-line transcript while streaming. Verify with GPUI's frame timing in a benchmark scene (`loft-ui/benches`).

### 3.5 Performance budgets (checked from M0 onward)

| Metric | Target |
|---|---|
| Cold start to interactive window | < 300 ms (daemon already running) |
| Daemon receives a CLI delta → pixel | < 16 ms p95 |
| Typing latency in the composer | < 8 ms |
| Memory | UI < 250 MB with 20 open threads; daemon < 150 MB plus agent processes |

---

## 4. Storage ([ADR-0006](adr/0006-storage-sqlite-event-log-toml.md))

### 4.1 Data home `~/.agentloft/`

```
config.toml                   # user config (hand-editable; UI writes via toml_edit preserving comments)
agentloft.db (+ -wal, -shm)   # SQLite, owned exclusively by agentloftd
backups/                      # pre-migration VACUUM INTO snapshots (keep last 5)
worktrees/<project-slug>/<thread-slug>/
attachments/<sha256>.<ext>    # pasted images etc. (content-addressed)
run/agentloftd.sock  run/agentloftd.lock  run/agentloftd.pid
logs/agentloftd.log  logs/agentloft.log   # tracing JSON, daily rotation, 7 days
cache/                        # model lists, probe results
```

`AGENTLOFT_HOME` overrides the location. Tests and `cargo xtask dev` always use an isolated home. This lesson comes from Ghostex: a dev daemon sharing a socket with the real one once killed real sessions.

### 4.2 Schema sketch

```sql
projects(id PK, name, root_path UNIQUE, settings_json, created_at, archived_at)
threads(id PK, project_id FK, title, harness_id, model, effort, permission_mode NULL,
        workspace_kind, worktree_path, branch, base_ref, native_session_id,
        parent_thread_id, parent_turn_id, fork_kind, status, created_at, updated_at, archived_at)
thread_events(seq INTEGER PK AUTOINCREMENT, thread_id, turn_id, item_id, type, payload_json, at, command_id)
turns(id PK, thread_id, ordinal, status, started_at, ended_at, usage_json, checkpoint_before, checkpoint_after)
items(id PK, thread_id, turn_id, ordinal, kind, status, payload_json, native_id, updated_at)
runtime_requests(id PK, thread_id, turn_id, item_id, kind, options_json, payload_json, status, decision_json, opened_at, resolved_at)
checkpoints(thread_id, ordinal, ref_name, commit_sha, turn_id, PRIMARY KEY(thread_id, ordinal))
command_receipts(command_id PK, result_json, at)
processes(id PK, pid, pgid, started_at_ns, kind, thread_id, terminal_id)   -- for the reaper
terminals(id PK, scope, project_id, thread_id, cwd, shell, status)
drafts(thread_id PK, text, attachments_json, updated_at)
ui_state(client_key, key, value_json, PRIMARY KEY(client_key, key))      -- per-window UI state, dock sizes
window_groups(id PK, client_key, name, tabs_json, active_tab, frame_json, ordinal, closed_at)
paired_devices(id PK, name, public_key, tailscale_login NULL, scopes, created_at, last_seen_at, revoked_at)
remote_audit(seq PK, device_id, at, request_kind, summary)
```

Pragmas: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`. Migrations are embedded (`rusqlite_migration`), forward-only, with a backup taken first.

### 4.3 Event log vs CRUD

This is a **hybrid**:
- **Projects, settings and UI state are plain CRUD.**
- **Thread timelines are event-sourced.** `thread_events` is append-only and kept forever. It is the transcript, and it is small. The `turns`, `items` and `runtime_requests` projections are updated in the same transaction and **can be rebuilt** from the log (`xtask rebuild-projections`).

Why not full event sourcing? Projects and settings gain nothing from it.

Why not pure CRUD? The log gives:
- reconnect replay (`after_seq`)
- idempotency
- deterministic tests (decider in, events out)
- easy debugging ("what happened in this turn")
- cheap fork and handoff generation

Event-sourced designs often prune the log and treat projections as the truth. We keep ours, because a desktop app's transcripts are the product.

### 4.4 Delta coalescing

Text and output deltas are **not** individual rows. The daemon keeps in-progress item buffers in memory, broadcasts each delta live, and writes an item snapshot about every 250 ms and at `ItemCompleted`. A crash loses at most about 250 ms of streamed text. That text is still recoverable from the native session on resume. Command output is capped per item (default 1 MB stored; full output stays in the CLI's own log).

### 4.5 Config format: TOML

```toml
# ~/.agentloft/config.toml
[ui]
theme = "system"            # system | light | dark | <theme name>
open_thread_in = "current-group"  # current-group | new-window

[context]
cross_thread_scope = "project"    # project | all | off  (Agentloft MCP server, ADR-0013)

[remote]                          # ADR-0014 — off by default
enabled = false
mode = "tailscale-serve"          # tailscale-serve | tailnet-bind | ssh-only
port = 47321                      # loopback port fronted by `tailscale serve`
allowed_tailscale_logins = []     # optional extra allowlist, e.g. ["you@github"]

[worktrees]
root = "~/.agentloft/worktrees"
default_mode = "ask"        # ask | always | never  (decided: ask)
branch_prefix = "agentloft/"

[harness.claude]
path = "auto"               # or absolute path
default_model = "auto"      # auto = harness default
default_permission_mode = "harness-default"
idle_timeout_minutes = 15

[harness.codex]
path = "auto"
default_permission_mode = "harness-default"
process_mode = "shared"     # shared | per-thread

[[harness.acp]]
id = "gemini"
command = "gemini"
args = ["--acp"]

[[harness.acp]]
id = "opencode"
command = "opencode"
args = ["acp"]
```

Per-project config lives in an optional `<repo>/.agentloft/config.toml`. It holds the worktree setup script, files to copy into new worktrees (`.env*`), the project's actions (`[[actions]]`: commands run from a button on the thread), and default harness and model. The keymap is `~/.agentloft/keymap.toml`.

---

## 5. Git, worktrees, process lifecycle ([ADR-0008](adr/0008-git-worktrees-checkpoints.md))

### 5.1 Git via CLI

`loft-git` shells out to `git`, with `--porcelain=v2 -z` parsing, `GIT_OPTIONAL_LOCKS=0` for status polling, and timeouts. Zed, Vibex and Ghostex all do this. Git behaves exactly as the user's git does (hooks, config, credential helpers, LFS). `gix` may be added later for hot read-only paths (status, blob reads) if profiling says so.

### 5.2 Worktrees

**Create** (thread setting "Run in worktree", default from config):
1. Run `git worktree add -b agentloft/<thread-slug> ~/.agentloft/worktrees/<project-slug>/<thread-slug> <base>`. `<base>` is the current HEAD, or a chosen branch/ref.
2. Copy the configured untracked files (`.env*`).
3. Run the project setup script in a PTY. Its output is shown in the thread as a `notice`/command item.
4. The thread's `cwd` is the worktree.

**Actions on a worktree thread:**
- Open in Finder or editor.
- **Merge back (local).** Squash-merge into the base branch, run in the main repo. A dry-run conflict check (`git merge-tree --write-tree`) runs first. The commit message is drafted from the thread title and turns, and the user can edit it.
- **Create PR.** Push `agentloft/<slug>` and run `gh pr create`. The title and body are drafted from the digest. Disabled with a hint if `gh` is missing or not authenticated.
- Both actions sit in the thread header's ⎇ menu.

**Archive:** if the worktree is clean, `git worktree remove` it and keep the branch (optionally delete it). If it is dirty, confirm and offer to commit a WIP snapshot first. `git worktree prune` runs on startup.

### 5.3 Checkpoints, diffs and revert

- **Capture** at the start and end of every turn. The user's index and branch are never touched:

  ```
  GIT_INDEX_FILE=<tmp> git add -A
  git write-tree
  git commit-tree <tree> -p HEAD
  git update-ref refs/agentloft/checkpoints/<thread-id>/<n> <commit>
  ```
- **Diffs:**
  - turn diff: `git diff <before> <after>`
  - cumulative: `git diff <thread-base> <latest>`
  - Patch text is fed to the gpui-kit `Diff` component.
- **Revert to turn N:** restore the worktree from checkpoint N (`git read-tree` + `checkout-index` into the worktree, with a confirmation that lists the files that will change). Then call the provider's native revert if supported: Codex `thread/revert`; Claude `rewind_files`, which reverts files only, so the conversation continues with a notice. Otherwise insert a `notice` item telling the agent the files were reverted.
- **Garbage:** checkpoint refs are deleted when the thread is deleted; archiving keeps them.

### 5.4 Process supervision

- Every child (agent CLIs, setup scripts, PTY shells) is spawned in its **own process group**: `CommandExt::process_group(0)`, or `setsid` for PTYs.
- Each child is recorded in the `processes` table with its pid, pgid and start time.
- **Graceful stop:**
  1. Protocol-level shutdown: close stdin. `claude -p` and `codex app-server` exit on EOF.
  2. After 2 s: `SIGTERM` to `-pgid`.
  3. After 5 s more: `SIGKILL` to `-pgid`.
- **Interrupt** always uses the protocol message (control `interrupt`, `turn/interrupt`, `session/cancel`), never signals. SIGINT is the last resort for Claude.
- **Startup reaper:** for each row in `processes`, if the pid is alive *and* its start time matches (guarding against PID reuse; `proc_pidinfo` on macOS), kill the group. Then clear the table.
- **Daemon death:** children see EOF on stdin and exit on their own. The reaper covers the rest. On Linux, `PR_SET_PDEATHSIG` is also used. On Windows, Job Objects with `KILL_ON_JOB_CLOSE` (later), following Ghostex's `ProcessTree`.
- **Harness crash policy:** a crashed harness process is restarted lazily on the next command, at most 3 times per 10 minutes per thread. After that the thread shows an error state with the logs.

### 5.5 PTY

- **Daemon side:** `loft-pty` uses `portable-pty` 0.9 (openpty / ConPTY). There is one reader task per PTY. Output goes to a per-terminal **ring buffer** (default 8 MB) and is broadcast to subscribers as `TerminalOutput` chunks. Resize and input arrive as commands.
- **UI side:** parse with libghostty-vt (`loft-terminal`; `alacritty_terminal` stays a fallback behind `TerminalBackend`) and render with a custom GPUI element.
- **On reconnect** (UI restart), replay the ring buffer into a fresh `Term`.
- Terminals survive UI restarts. They do not survive daemon restarts until session holders exist (M9).

### 5.6 Crash recovery matrix

| Crash | Recovery |
|---|---|
| UI | Nothing is lost. On relaunch it reconnects, resubscribes with `after_seq`, restores windows and tab groups from `ui_state`, and replays terminal buffers. |
| Daemon | launchd (M7+) or the UI (M0–M6) restarts it. The reaper kills orphans. Turns still `running` become `interrupted` with a notice. Open approvals become `expired`. Items are restored from the last snapshot. Native sessions reopen lazily on the next message. |
| Agent CLI | The session emits `SessionExited`. The turn is marked `failed` with stderr tail and logs. The next message reopens it with resume, subject to the restart cap. |
| Machine reboot | The same as a daemon crash. Threads are idle and resumable. |

### 5.7 Remote access over Tailscale or SSH ([ADR-0014](adr/0014-remote-access-tailscale.md))

**Goal:** from a laptop on the go (and later a phone), drive the agents running on your desktop, using **your own Tailscale tailnet**. Nothing is ever exposed to the public internet.

**Host side (`agentloftd`)**
- Remote access is off by default. Settings → Remote → Enable.
- The daemon then starts a **WebSocket listener** carrying the same frames as the Unix socket. In the default mode it binds to **127.0.0.1 only**.

**Connection modes**

| Mode | How | When |
|---|---|---|
| **`tailscale-serve`** (default) | Agentloft runs `tailscale serve --bg --https=<port> http://127.0.0.1:<port>`. Tailscale terminates TLS with the tailnet's certificate (`https://<host>.<tailnet>.ts.net:<port>`) and proxies to loopback. The daemon never listens on a network interface itself. | Normal use. TLS and identity headers come for free. |
| `tailnet-bind` | The daemon binds directly to the Tailscale interface address (100.x / fd7a:…). The transport is WireGuard-encrypted by Tailscale. | When `tailscale serve` isn't available, for example with some managed tailnets. |
| `ssh-only` | No listener. The client runs `ssh -L <local.sock>:~/.agentloft/run/agentloftd.sock host`; OpenSSH forwards Unix sockets. | Zero new attack surface. Works without Tailscale. |

**Safety**
- Agentloft **refuses to run while Tailscale Funnel exposes the port**. It checks `tailscale funnel status` / `serve status` at enable time and every 5 minutes, and shuts the listener down if the port becomes public.

**Authentication: two independent factors**
1. **Network identity.** In `tailscale-serve` mode, Tailscale adds `Tailscale-User-Login` / `Tailscale-User-Name` headers to proxied requests. In `tailnet-bind` mode, the daemon asks the Tailscale LocalAPI `whois` about the peer IP. Either way, the login is checked against `allowed_tailscale_logins`, which defaults to the host's own tailnet login.
   - Headers are trusted only on the loopback listener that Tailscale fronts. Factor 2 is still required, because any local process could forge them.
2. **Device pairing.**
   - Each client device generates an ed25519 key.
   - The first connection shows a 6-digit code on both screens, and the user approves it on the host (in-app prompt). The public key is then stored in `paired_devices`, with a name and scopes.
   - Every later connection completes a signed challenge (nonce plus timestamp) in `Hello`.
   - Revoke a device in Settings → Remote.

**Authorization**
- Per-device scopes: `full` (default) or `read-only` (watch and approve only).
- Every remote command is appended to `remote_audit`.
- Remote clients can answer approvals. This is the main mobile use case: "approve `npm test` from your phone".
- Pairing attempts are rate-limited, and unpaired connections are dropped after the handshake.

**Client side**
- The same Agentloft app adds **Environments** (Local, plus `desktop.tailnet` and others).
- Threads and window groups can hold threads from any environment. Each tab shows an environment badge.
- Behaviour that differs remotely:
  - the project folder picker browses the *host* filesystem through a daemon `fs/list` query;
  - "Open in Finder/editor" is disabled for remote threads, or offered as `code --remote ssh-remote+host`;
  - attachments are uploaded over the protocol;
  - terminals work unchanged.
- Bandwidth: WebSocket `permessage-deflate`, plus delta coalescing at the host (every 30–50 ms over remote links).
- Reconnects use `after_seq`, the same as locally.

**Not in scope:** our own relay or cloud service (we rely on Tailscale or SSH), embedding `tsnet` (Go, not Rust), and Funnel or other public exposure.

> Unverified until implementation: header injection by `tailscale serve` for HTTPS proxies on the installed Tailscale version; LocalAPI socket access from the macOS App Store variant of Tailscale; the exact `serve --bg` flag syntax. `probe_tailscale()` will detect which modes are available.

---

## 6. Risks and unknowns

| Risk | Likelihood / impact | Mitigation |
|---|---|---|
| **GPUI API churn.** Pre-1.0. The crates.io `gpui` (0.2.2) is a year stale. `gpui-pre` snapshots ship weekly and are maintained by one third party (Longbridge). | High / Medium | Follow gpui-kit's exact `gpui-pre` pin. Bump every 2–4 weeks in a dedicated PR. Keep GPUI types out of non-UI crates. Escape hatch: `[patch]` to a pinned zed git rev or our own fork, as Ghostex and tty7 do. |
| **gpui-kit churn and bugs** (weekly releases, 0.x) | Medium / Medium | Wrap the components we use behind thin `loft-ui` widgets. Be ready to fork, as Ghostex does. Contribute fixes upstream. |
| **Text input and IME maturity** | Medium / High (the composer is core) | gpui-kit `Input`/`Editor` claim preedit-aware IME. **Test Japanese (Kotoeri), Chinese (Pinyin) and Korean in M0**, along with emoji, dead keys and undo. Fallback: a custom input built on GPUI's `EntityInputHandler`, using `examples/input.rs` as reference. |
| **Streaming markdown cost** (TextView re-parses on `push_str`) | Medium / Medium | Stable-blocks plus live-tail splitting (§3.4). Benchmark scene from M1. |
| **Claude control protocol is undocumented** and the CLI auto-updates | Medium / High | Tolerant parsing, per-version fixtures, nightly canary, version range in `probe()`. A TS-SDK sidecar fallback is designed but not built. |
| **Codex app-server is marked "experimental"** and changes often (codex 0.167 adds new methods weekly) | Medium / Medium | Generated types from the pinned schema, a schema-diff canary, and ignoring unknown notifications. |
| macOS GUI `PATH` and CLI auth state | High / Low | Login-shell environment capture, binary path overrides, `probe()` showing auth status with a "Run `claude login` in terminal" action. |
| **Daemon complexity** (IPC, versions, upgrades) | Medium / Medium | Small protocol surface, protocol version N/N−1, `xtask test-daemon` with an isolated home, and end-to-end tests that drive the client library headlessly. |
| Cross-platform | Medium / Medium | macOS first. CI also runs `cargo check --target x86_64-unknown-linux-gnu` on non-UI crates from M0. Linux (GPUI wgpu, Wayland/X11) is M9; Windows (ConPTY, Job Objects, named pipe instead of UDS) after that. |
| **Packaging and distribution** (deferred by decision) | Low now | The interim distribution is an unsigned or ad-hoc signed `.app`/`.dmg` (users must right-click → Open, or run `xattr -d com.apple.quarantine`). Later: Developer ID with hardened runtime and `notarytool`; Sparkle 2 through a small ObjC shim (pattern from Ghostex, MIT) or a custom ed25519-signed appcast. The updater must also apply the daemon-upgrade flow (§1.2). |
| Licensing contamination | Low / High | `cargo-deny` allowlist. Never copy from Zed's GPL crates (`editor`, `terminal`, `markdown`, `ui`, `theme`) or Vibex (AGPL). Attribute MIT/Apache borrowings in `THIRD_PARTY_NOTICES.md`. |
| **Remote access security** (ADR-0014) | Low / High | Off by default. Binds to loopback behind `tailscale serve` (or the tailnet interface). Refuses to run under Funnel. Requires two factors (tailnet identity plus paired device key). Per-device scopes, an audit log, rate-limited pairing. `ssh-only` mode for zero listening surface. Security review before release. |
| **Cross-thread reading leaks context** (ADR-0013) | Medium / Medium | Read-only tools. Same-project scope by default, `off` setting, per-thread toggle. Tool calls are visible in the timeline. Archived threads are excluded by default. |
| Name and trademark ("Agentloft") | Medium / Medium | AgentLoft.com is an unrelated real-estate marketing product, and agentloft.ai is registered by someone. Run a USPTO/EUIPO search. The GitHub org `github.com/agentloft` has been claimed. Register agentloft.dev or .app. |
| Compile times (GPUI + gpui-kit + tree-sitter) | High / Low | Split crates, `opt-level=1` for dependencies in dev, the `sccache` and `mold`/default ld64 settings, and an `xtask dev` that only rebuilds what changed. |

---

## 7. Milestones

Each milestone ends with a demo plus the listed automated tests passing in CI (macOS runner: fmt, clippy `-D warnings`, nextest, cargo-deny). There are no calendar dates; sequence matters more than speed.

### M0: Walking skeleton (daemon + window + one Claude thread end to end)

**Scope:**
- Workspace and crates per §1.3, CI, `deny.toml`.
- `agentloftd`: socket, handshake, single-instance lock, SQLite with the first migration, login-shell environment capture.
- `agentloft`: one window with a sidebar and a thread view.
- Claude harness: text turns, tool-call rows, a minimal approval card, interrupt and resume.

**Acceptance criteria:**
1. `cargo xtask dev` builds both binaries, starts `agentloftd` with `AGENTLOFT_HOME=./.dev-home`, and opens the window.
2. **Add project** (folder picker) lists the project. **New thread** creates a Claude thread. Both survive relaunch.
3. Composer: multi-line. Enter sends, Shift+Enter adds a newline. Japanese IME composition commits correctly.
4. A prompt streams token by token into the timeline as markdown. Tool calls appear as one-line rows with running and done status. The turn ends with cost and usage shown.
5. With Claude's *default* permission mode (no flag passed), a Bash tool request shows an approval card. **Allow** runs the command; **Deny** shows Claude's denial handling.
6. **Stop** interrupts within 2 s, and the turn shows as `interrupted`.
7. Quitting the UI mid-turn leaves the turn running (`pgrep -f "claude -p"` is alive). Reopening shows the caught-up timeline and the live stream continues.
8. After `kill -9` of `agentloftd` mid-turn, the UI respawns the daemon. The turn shows `interrupted`. The next message resumes the **same** Claude session: the agent remembers earlier context and `native_session_id` is unchanged.
9. After daemon shutdown, no `claude` process spawned by Agentloft remains. The reaper test passes.
10. Automated tests:
    - decider unit tests
    - protocol round-trip tests
    - Claude adapter fixture replay (fixtures recorded with claude 2.1.319)
    - a headless end-to-end test, `loft-client` → `agentloftd` → `FakeHarness`

### M1: Claude complete and a rich timeline

**Scope:**
- Full approval options (once, session, deny, deny+stop) with keyboard shortcuts.
- Permission-mode picker and per-harness defaults in `config.toml`.
- Model and effort picker (`set_model`).
- Thinking display.
- Full tool cards (command output, file-change inline diff, read and search, MCP, subagent nesting, todo list).
- Code highlighting and stable-block streaming markdown.
- Slash-command popover (from init).
- `@file` mentions.
- Image paste and drop.
- Queued messages.
- Drafts.
- Crash-recovery matrix (§5.6) implemented.

**Acceptance criteria:**
- Every Claude fixture scenario (approval for each tool kind, subagent, TodoWrite, image input, plan mode) renders correctly.
- Switching mode or model mid-thread takes effect on the next tool call or turn.
- The 10k-line streaming benchmark holds ≥ 60 fps.
- All recovery-matrix rows are covered by tests.

### M2: Codex and harness parity

**Scope:**
- `loft-harness-codex` with generated types.
- Approvals with all decision types, including execpolicy amendment ("always allow `npm test`").
- Steer.
- `model/list` and effort.
- Reasoning summaries.
- Harness picker when creating a thread.
- Same-provider **native fork** for Claude and Codex, without worktrees yet.

**Acceptance criteria:**
- The identical scenario suite passes against both adapters through the canonical events.
- Two threads, one Claude and one Codex, stream concurrently in one project.
- Forking at turn N produces a thread whose agent recalls turns 1..N only.
- Killing `codex app-server` mid-turn yields `failed` → resume works.

### M3: Worktrees, checkpoints, diffs and cross-provider fork

**Scope:**
- Per-thread worktrees with setup script and file copying.
- Per-turn checkpoints.
- Diff dock (turn, cumulative, against any checkpoint).
- Revert to turn.
- Archive and cleanup.
- Merge back and "Create PR" through `gh`.
- **Cross-provider fork with handoff** (deterministic digest) into a new worktree from the parent's checkpoint.
- **Agentloft MCP server** (§2.9), attached to Claude and Codex: `list_threads`, `read_thread`, `search_threads`, `get_thread_diff`, `get_thread_plan`, plus scope settings.

**Acceptance criteria:**
- Two worktree threads edit the same file concurrently without interference.
- The turn diff exactly matches `git diff` of the checkpoint refs.
- Revert restores the files byte for byte and calls `thread/revert` on Codex.
- Archiving a dirty worktree prompts first.
- Worktree threads use the default `ask` prompt, create `agentloft/<slug>` branches, and both merge-back paths work: a local squash-merge, and `gh pr create` against a test repo.
- A Claude→Codex fork produces a Codex thread whose first message is the editable handoff and whose worktree matches the parent at turn N.
- In a Codex thread, asking "what did the Claude thread decide about X?" makes the agent call `read_thread` and answer from it. With `cross_thread_scope = off`, the tools are absent. A thread in another project is not visible at `project` scope.

### M4: Integrated terminal

**Scope:**
- `loft-pty` in the daemon.
- Ghostty (libghostty-vt) element in the UI.
- Tabs per thread or project; cwd set to the thread's workspace.
- Selection and copy, URL detection, IME, scrollback, search.
- Resize.
- Ring-buffer replay on UI restart.

**Acceptance criteria:**
- `vim`, `htop`, `less` and an interactive `claude` TUI render and accept input correctly.
- `cat` of a 50 MB file keeps the UI responsive (no frame > 50 ms) and finishes within 2× of Terminal.app.
- Terminals survive a UI restart with scrollback intact.
- Closing a tab kills its process group, with no orphans.

### M5: Windows, groups and workspace UX

**Scope:**
- Multiple windows.
- **User-defined window groups**, not tied to projects: native tabs via a per-group `tabbing_identifier`. New, rename, close and reopen groups; move a tab between groups.
- Window and tab-group restore.
- Command palette.
- Settings window (harness status and paths, defaults, worktrees, appearance, keymap).
- Notifications.
- Thread search (SQLite FTS5 over items).
- Needs-attention inbox.

**Acceptance criteria:**
- A group named "Auth refactor" holding a Claude thread from project A and a Codex thread from project B as tabs survives an app restart with the same tabs, order and frame.
- Dragging a tab into another group's window moves it, and the change persists.
- The same thread open in two windows stays in sync while streaming.
- Every action is reachable from the palette.
- Settings changes round-trip to `config.toml` with comments preserved.

### M6: ACP harnesses

**Scope:**
- `loft-harness-acp` with Gemini CLI and OpenCode presets plus user-defined ACP agents.
- Client-side `fs/*` and `terminal/*` implementations.
- Capability-driven UI: hide steer and revert when they are unsupported.

**Acceptance criteria:**
- The core scenario suite (text, tool approval, deny, interrupt, resume) passes for Gemini and OpenCode.
- Adding a custom ACP agent through `config.toml` works without a code change.

### M7: Distribution (unsigned or ad-hoc for now) and robust daemon lifecycle

**Scope:**
- `.app` bundle containing both binaries.
- `SMAppService` LaunchAgent registration.
- The daemon-upgrade protocol (§1.2), end to end.
- An in-app updater (with signature verification once signing exists).
- Onboarding: detect CLIs, versions and auth, with a "login" action that opens a terminal.
- Local crash reports.

**Acceptance criteria:**
- Updating the app while a Claude turn runs does not interrupt it (UI-only update).
- A daemon update applies automatically when idle.
- A fresh machine with only `claude` installed reaches a working thread in under 60 s.

### M8: Remote access (Tailscale and SSH)

**Scope:**
- WebSocket listener (loopback).
- The `tailscale-serve`, `tailnet-bind` and `ssh-only` modes, plus a Funnel guard.
- Device pairing (ed25519, 6-digit code) and per-device scopes.
- Audit log.
- Environments in the client, remote `fs/list`, and remote attachments.
- Settings → Remote (enable, devices, revoke).
- An external security review of the listener and pairing flow.

**Acceptance criteria:**
- From a second Mac on the same tailnet, pair, open a group mixing local and remote threads, stream a turn, and approve a tool call remotely.
- Revoking the device drops its connection within 1 s and blocks reconnects.
- With Funnel enabled on the port, the listener refuses to start and the UI explains why.
- An unpaired client, or a client on a non-allowlisted tailnet login, is rejected and the attempt is logged.
- `ssh-only` mode works with plain OpenSSH and no Tailscale installed.

### M9: Cross-platform and durability (by demand)

**Scope:**
- Linux build (GPUI wgpu, Wayland/X11, PDEATHSIG).
- Windows build (ConPTY, Job Objects, named pipes).
- `agentloft-hold` session holders so turns survive daemon restarts (see §8).

**Acceptance criteria:** defined when this milestone is scheduled.

---

## 8. Open questions

**Decided so far:**
- **Platform and stack:** Apache-2.0, a Rust-only stack, daemon from M0, macOS first, `~/.agentloft`, direct download.
- **Agent behaviour:**
  - the harness's own default permission mode;
  - threads pinned to one provider, with forks to any provider;
  - cross-provider handoff by deterministic digest, plus cross-thread reading through MCP;
  - worktree default `ask`, with `agentloft/<slug>` branches;
  - merge-back by local squash or `gh` PR.
- **UI:** user-defined window groups not tied to projects.
- **Remote:** remote access over Tailscale or SSH (design accepted, M8).
- **Telemetry and import:** none now. A `loft-telemetry` no-op trait is the seam for later.
- **GitHub:** the org `github.com/agentloft` is claimed. The repo lives at `github.com/agentloft/agentloft`.

**Still open:**

1. **Domain and bundle ID.** Register `agentloft.dev` or `.app`. The proposed bundle ID is `dev.agentloft.Agentloft`.
2. **Trademark check** for "Agentloft". There is an unrelated real-estate SaaS at agentloft.com.
3. **Session holders.** These let turns survive a *daemon* restart; UI restarts are already covered. Today, if `agentloftd` restarts mid-turn (a crash, or a forced update while busy), the agent's stdin/stdout pipes close and the agent exits. The turn is lost, but the conversation resumes by ID.
   - **What a holder is:** a tiny `agentloft-hold` process per running agent (or terminal shell) that owns those pipes, buffers output while the daemon is away, and lets the restarted daemon reconnect and replay, including any pending approval. It is about 300 lines, almost never changes, and costs about 2 MB per agent.
   - **When it matters:** only for daemon crashes and forced daemon updates mid-turn. Normal daemon updates already wait until idle.
   - **Recommendation:** keep it in M9 unless you plan long unattended or overnight runs where updates shouldn't wait for idle, or you see daemon crashes in practice.
4. **Agent-to-agent write tools** (`post_to_thread`, `spawn_thread`) on the Agentloft MCP server: v1 is read-only. Decide after M3 whether agents may message or spawn other threads, behind approvals.

---

## Appendix A: References and what we borrow

| Source | License | Use |
|---|---|---|
| [Ghostex](https://github.com/maddada/Ghostex) (local `~/Desktop/dev/Ghostex`) | MIT | **Anti-pattern:** TUI scraping. **Patterns:** GPUI terminal element design (`apps/desktop/src/terminal_element.rs`, `terminal_model.rs`), process-group and Job Object kill (`server/src/platform/process.rs`), self-heal caps, Sparkle shim, notarization script, isolated test-daemon rule. |
| [Vibex](https://github.com/vibex-ai/vibex) | **AGPL-3.0** | Ideas only. **Do not copy code.** Validates ACP + `alacritty_terminal` + rusqlite + git CLI + gpui-kit as a stack. |
| [Wake](https://github.com/iAmCorey/Wake), [tty7](https://github.com/l0ng-ai/tty7) | MIT, Apache-2.0 | GPUI app structure, pinning approaches, persistent terminals. |
| [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), `gpui_tokio` | Apache-2.0 | UI framework. `gpui_tokio` may be vendored with attribution. |
| Zed `editor`, `terminal`, `terminal_view`, `markdown`, `ui`, `theme` | **GPL-3.0** | Do not copy or depend on. |
| [gpui-kit / gpui-component](https://github.com/longbridge/gpui-kit) | Apache-2.0 | Component library (Sidebar, Dock, MessageScroller, TextView, Diff, Input/Editor, Command, Settings). |
| [alacritty_terminal](https://crates.io/crates/alacritty_terminal), [portable-pty](https://crates.io/crates/portable-pty) | Apache-2.0, MIT | Terminal. |
| [agent-client-protocol](https://github.com/agentclientprotocol/rust-sdk) | Apache-2.0 | ACP harness. |
| [Codex app-server](https://github.com/openai/codex/tree/main/codex-rs/app-server) | Apache-2.0 | Protocol; schema generated locally with `codex app-server generate-json-schema`. |
| [Claude Agent SDK (Python/TS)](https://github.com/anthropics/claude-agent-sdk-python) | MIT | Reference implementation of the stream-json control protocol. |

## Appendix B: Environment and versions verified (2026-11-08)

- **Machine:** macOS 26.5.1 arm64; rustc/cargo 1.99.0; git 2.56.0; gh 2.102.0.
- **Agent CLIs:** `claude` 2.1.319, `codex-cli` 0.167.0, `opencode` 2.2.5. `gemini` is not installed.
- **Crates:** `gpui` 0.2.2 (crates.io, Oct 2025); `gpui-pre` 0.3.12 (weekly); `gpui-kit`/`gpui-component` 0.9.0 (pins `gpui-pre =0.3.11`); `alacritty_terminal` 0.26.0; `portable-pty` 0.9.0; `agent-client-protocol` 5.0.0 / `-schema` 1.15.1; `rusqlite` 0.41.x; `tree-sitter` 0.27.x.
- **GPUI source:** `WindowOptions.tabbing_identifier` and `SystemWindowTabController` are present in zed `main`. `gpui_platform` was split from `gpui` (Feb 2026). Linux moved from Blade to wgpu (Feb 2026).
- **Claude CLI flags confirmed via `claude --help`:**
  - `--output-format stream-json`, `--input-format stream-json`, `--include-partial-messages`, `--replay-user-messages`
  - `--permission-mode {acceptEdits,auto,bypassPermissions,manual,dontAsk,plan}`
  - `--permission-prompts {host,none}`, `--permission-prompt-tool`
  - `--resume`, `--session-id`, `--fork-session`, `--model`, `--mcp-config`, `--json-schema`
- **Codex:** method, request and notification names were taken from `codex app-server generate-json-schema`. `thread/fork` accepts `lastTurnId` and `cwd`; `thread/revert` takes `beforeTurnId`.
- **Unverified, to check during M0–M2:**
  - The exact `--resume-session-at` semantics.
  - The exact ACP flags for each long-tail agent.
  - OpenCode 2.x HTTP API details.
