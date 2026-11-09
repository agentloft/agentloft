# Milestone tracking

Local, checked-in tracker for [PLAN §7](PLAN.md#7-milestones). Update it in the same PR as the work.
Legend: `[x]` done and verified · `[~]` partial · `[ ]` not started.

## M0: Walking skeleton (in progress, branch `m0/scaffold`)

| # | Acceptance criterion | Status | Evidence |
|---|---|---|---|
| 1 | `cargo xtask dev` builds both binaries and runs against `./.dev-home` | [x] | `xtask/src/main.rs` |
| 2 | Add project (folder picker) and new thread; both survive relaunch | [x] | Folder picker in sidebar; persistence covered by `loftd/tests/e2e.rs` |
| 3 | Multi-line composer: Enter sends, Shift+Enter newline; Japanese IME commits correctly | [~] | Enter and Shift+Enter work; **IME test still manual and pending** |
| 4 | Token-by-token streaming markdown; tool rows with status; cost and usage shown | [x] | Live screenshot 2026-11-09; header shows the last turn's cost and tokens |
| 5 | Claude default mode: approval card, Allow runs the tool, Deny is handled | [x] | `live_claude` test, plus a UI screenshot of the approval card |
| 6 | Stop interrupts within 2 s | [x] | `live_claude`: interrupt took 14 ms |
| 7 | Quitting the UI mid-turn keeps the turn running; reopening catches up | [x] | Scripted check 2026-11-09: UI `kill -9` mid-turn; the turn completed in the daemon; reconnect gets a full snapshot (e2e) |
| 8 | `kill -9` of the daemon: UI respawns it, turn shows interrupted, the next message resumes the same session | [x] | Scripted check 2026-11-09: daemon `kill -9` mid-turn; the UI respawned it within seconds; the turn showed interrupted; the next message completed on the same session. Also `live_claude` (restart) and `loft-core` tests |
| 9 | No orphan `claude` processes after shutdown; reaper | [x] | Process groups, graceful stop and the startup reaper are in place. Scripted check: after a daemon `kill -9`, the agent child (`opencode acp`) exited at once on stdin EOF. The reaper test (`loftd` reaper) kills a recorded orphan group and spares a reused pid |
| 10 | Tests: decider/core, protocol round-trip, Claude fixtures (2.1.319), headless e2e | [x] | 27 tests across crates (plus the ignored `live_claude`); fixtures in `crates/loft-harness-claude/fixtures/` |

Infrastructure:
- [x] CI: macOS (fmt, clippy, tests), Linux headless tests, cargo-deny
- [x] `deny.toml` dependency policy (ADR-0009)
- [x] CONTRIBUTING (DCO), PR and issue templates
- [x] Branch protection on `main`: PR + 3 required checks, no force-push

## M0.5: Design pass — "professional UI" (in progress, branch `m0.5/design`)

Goal: visual quality on par with Codex and Claude Desktop, plus appearance customization, without regressing performance.

- [x] Design tokens: refined neutral palettes for light and dark, radii, elevation; follows the system appearance
- [x] Appearance settings in `config.toml` `[ui]`: theme, accent, UI and mono fonts, sizes, density, content width; live reload (verified)
- [x] Integrated title bar (transparent, traffic lights inset), breadcrumb header
- [x] Sidebar: search, project groups, thread rows with relative time, running spinner and approve badge, collapsible (⌘B)
- [x] Conversation column: centered max width, prose replies, user bubbles, grouped tool activity that expands on click, turn footers
- [~] Floating composer card: model and mode chips, round send/stop button (attachments M1)
- [~] Approval card as a first-class prompt with the exact command (keyboard shortcuts pending)
- [x] Empty states: welcome, empty thread, reconnecting banner
- [x] Performance: palette computed once per frame, incremental row splicing, virtualized timeline. 10k-item thread (2,500 turns with Markdown, code and tools): daemon snapshot 144 ms (5.4 MB), opens at the latest turn, 0% CPU idle, ~320 MB RSS (debug build)

## Workbench (branch `feat/workbench`)

Built ahead of the PLAN's milestone order, by request.

| Area | Status | Notes |
|---|---|---|
| Codex harness (M2) | [x] | `codex app-server`; approvals follow `availableDecisions`; MCP elicitations; resume verified live (`live_codex`) |
| ACP harnesses (M6) | [x] | `loft-harness-acp`: OpenCode (`opencode acp`) and Gemini (`gemini --acp`, falls back to `--experimental-acp`). Covers streaming, reasoning, tool calls with diffs, plan, permissions (`allow_once/always`, `reject_*`), `session/cancel`, `session/load` with the history replay suppressed, and model/mode via `configOptions`. OpenCode 2.2.5 verified live (`live_opencode`): approval, interrupt, MCP, resume. **Not yet done:** Gemini untested (not installed); client `fs/*` and `terminal/*`; `[[harness.acp]]` custom agents in config; per-harness config lookup in `loft-core` (non-Codex harnesses read `[harness.claude]`); UI picker. Integrated into the app: picker, fork menus and palette (shown once the CLI is detected); Settings sections; image prompts when `promptCapabilities.image`; `[harness.opencode]` and `[harness.gemini]` config. OpenCode verified live through the app. The model picker lists the agent's own models, reported at session start (OpenCode: 31) |
| Harness picker / model / permission menus | [x] | Picker on empty threads; permission dropdown in the composer. **Model picker** (`loft-ui/src/model_picker.rs`): one popover with search, agent tabs (agent logos; locked once the thread starts), the agent's models and a reasoning-effort ladder (Auto + each level the model accepts), keyboard driven (↑↓ model, ←→ effort, ⏎, esc). Catalogs come from the agents: Claude's `initialize` control response via a short-lived `claude` with hooks off (aliases plus pinned versions, effort levels, default model), Codex `model/list` (efforts and default effort), ACP at session start. **Effort** (`SetEffort`, `Thread.effort`): Codex sends it on every `turn/start`; Claude gets `--effort`, and a change restarts the process between turns, resuming the session (verified live: `low` then `max`). File viewer opens already highlighted (parsed off the main thread) |
| Checkpoints + diffs (M3 subset) | [x] | Hidden refs per turn; Changes tab: this thread / last turn / uncommitted |
| Per-thread worktrees (M3) | [x] | Local / New worktree picker. The worktree is created on the first message (`agentloft/<slug-from-prompt>` under `~/.agentloft/worktrees`, `.env*` copied in). Squash merge-back runs a `merge-tree` conflict pre-check. Archive removes the worktree (confirms if dirty) and keeps the branch. **Create pull request** commits, pushes (with the user's login env) and runs `gh pr create` with a deterministic title and body, reusing an already-open PR. Verified live with Claude; the PR flow is tested against a bare remote and a stub `gh`. **Undo changes** on any turn footer restores files to that turn's before-checkpoint (removing new files, restoring changed or deleted ones) without touching the index or HEAD; the agent is told on its next turn. **Per-project setup:** `.agentloft/config.toml` `[worktree] setup` runs in the login shell before the first prompt, behind a live notice and before the turn's checkpoint. Failures are shown and the agent is told. `copy` globs bring untracked files along. **Base branch:** "From ⎇ main ▾ · Local | origin" under New worktree: a searchable branch list (`ListBranches`; main branch first, checked-out marked, local/remote availability), and local copy vs the remote's. The default is the main branch (`origin/HEAD`, else `main`/`master`) from `origin`, fetched first (offline: the last fetched copy). Branches start with `--no-track`; merge-back targets the branch name |
| Forks, native + cross-provider (M3) | [x] | Thread menu → Fork with Claude Code / Codex. Same harness forks natively (Claude `--resume --fork-session --session-id`; Codex `thread/fork`, falling back to a fresh thread). Another harness gets a deterministic handoff digest (requests, outcome, files changed, open todos, MCP pointer) pre-filled in the composer. Each fork gets its own worktree from the parent's current state. Forks nest under their parent and show a "Forked from" marker. All three paths verified live. **Not yet done:** fork from an arbitrary turn |
| File explorer + viewer | [x] | .gitignore-aware tree; tree-sitter highlighting; Markdown preview; images |
| Subagent view | [x] | Agent cards in the timeline; subagent tab with steps and report |
| Agentloft MCP server | [x] | `browser_*`, `list_threads`, `read_thread`, `get_thread_diff`; verified with Claude and Codex |
| Terminal (M4) | [x] | Ghostty VT (`loft-terminal`, libghostty-vt) on daemon PTYs with an 8 MiB replay; one shell per thread that reattaches after a UI restart; vim, truecolor and CJK verified live. Stress: 3M lines (~25 MB) in 1.9 s with the UI responsive; the daemon stays at ~28 MB because output is read from the ring and never queued on the bus. ⌘-click opens URLs: localhost dev servers in the side-panel browser, others in the default browser. Terminal tabs: several shells per thread (`thread:<id>#n`), rebuilt from the daemon after a UI restart. Closing the last tab starts a fresh shell, and archiving closes them all |
| Browser panel | [x] | WKWebView (`loft-browser`, wry) in the side panel; agents drive it over MCP (navigate, snapshot, click, type, eval); Claude verified live |
| Office documents | [x] | `loft-docs`: Word (paragraphs/tables), Excel (sheet grid), PowerPoint (slides), CSV; verified with sample files |
| Queued messages, approval shortcuts (M1) | [x] | ⌘⏎ / ⌘⇧⏎ / ⌘⇧⌫ |
| Composer completions (M1) | [x] | `/` at the start lists the agent's slash commands (persisted from session start, falling back to another thread of the same harness). `@` fuzzy-finds files in the thread's workspace (gitignore-aware index cached in the daemon, ranked by file name and segment starts). ↑↓, ⏎/⇥ to insert, esc to dismiss |
| Image attachments (M1) | [x] | Paste (⌘V), drag-and-drop or the paperclip. The daemon stores images under `~/.agentloft/attachments` (png/jpeg/gif/webp, at most 10 per message, 20 MB each). Claude gets base64 image blocks, Codex gets `localImage`. Timeline thumbnails open on click, and queued messages keep their images. Verified live with both agents |
| Windows and tab groups (M5) | [x] | New Window (⌘⇧N), New Tab (⌘T), and Open in New Tab from the thread menu. Each window has its own daemon connection and selection, so a tab group holds threads from any project. Uses native macOS tabs (`addTabbedWindow:`) with our own tab strip, since GPUI hides AppKit's under the transparent title bar. Window titles follow the thread. Browser calls go to a window that has the thread open. The Dock icon reopens a window. **Session restore:** windows, tab groups and each window's thread come back on relaunch (`ui-session.json`, saved on quit) |
| Thread management | [x] | Right-click a thread: Rename…, Open in New Tab, Fork with Claude Code/Codex, Archive. Renames go through the thread actor so they stick. An archived section in the sidebar has Restore, which re-creates a worktree thread's worktree from its kept branch |
| Command palette (M5) | [x] | ⌘K: fuzzy search over every live thread across projects (by project, agent and branch); actions on the current thread (stop, fork, open in tab, merge, PR, archive); app actions with their shortcuts. Thread search moved to ⌘⇧F |
| Attention (M5) | [x] | The Dock badge counts threads waiting for approval. The Dock icon bounces when a turn finishes or needs approval while the app is in the background. The sidebar shows an unread dot on threads that finished while you were elsewhere. In the app bundle, a Notification Center banner (one per thread, replacing the previous) says the agent finished, errored or needs approval. macOS asks for permission on first use; banners are wired but not yet seen on screen. The unbundled dev binary skips them |
| Settings window (M5) | [x] | ⌘, opens a native window with sections: Appearance (theme, accent swatches, text/code size, density, width, fonts; applied live), Agents (binary, default model and permissions per harness), Worktrees (mode, prefix) and About. Writes `config.toml` with toml_edit, keeping comments. The daemon applies changes via `ReloadConfig`. Agents also show whether each CLI is installed (path and version), and the welcome screen lists ready agents or how to install them. **Not yet done:** keymap editor, auth status |
| Daemon updates | [x] | The daemon reports its executable's mtime. When the binary on disk has changed (rebuild or app update), the app asks an idle daemon to shut down and starts the new one. A busy daemon keeps running until its agents finish. Unknown requests from a newer app get a clear error instead of hanging |
| App icon, bundle, About, menus | [x] | `cargo xtask bundle` |

## M1 → M9

See [PLAN §7](PLAN.md#7-milestones). Not started.
