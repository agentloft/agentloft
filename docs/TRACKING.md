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
| 7 | Quitting the UI mid-turn keeps the turn running; reopening catches up | [~] | Daemon independence is covered by e2e (reconnect gets a full snapshot); **manual quit-mid-turn check pending** |
| 8 | `kill -9` of the daemon: UI respawns it, turn shows interrupted, the next message resumes the same session | [~] | Recovery + resume verified by `live_claude` (restart) and `loft-core` tests; UI auto-respawns on disconnect; **manual kill -9 check pending** |
| 9 | No orphan `claude` processes after shutdown; reaper | [~] | Process groups + graceful stop + startup reaper implemented; **no automated reaper test yet** |
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
- [~] Performance: palette computed once per frame, incremental row splicing; 10k-item benchmark pending

## Workbench (branch `feat/workbench`)

Built ahead of the PLAN's milestone order, by request.

| Area | Status | Notes |
|---|---|---|
| Codex harness (M2) | [x] | `codex app-server`; approvals follow `availableDecisions`; MCP elicitations; resume verified live (`live_codex`) |
| Harness picker / model / permission menus | [x] | Picker on empty threads; model + mode dropdowns in the composer |
| Checkpoints + diffs (M3 subset) | [x] | Hidden refs per turn; Changes tab: this thread / last turn / uncommitted |
| Per-thread worktrees (M3) | [x] | Local / New worktree picker. The worktree is created on the first message (`agentloft/<slug-from-prompt>` under `~/.agentloft/worktrees`, `.env*` copied in). Squash merge-back runs a `merge-tree` conflict pre-check. Archive removes the worktree (confirms if dirty) and keeps the branch. **Create pull request** commits, pushes (with the user's login env) and runs `gh pr create` with a deterministic title and body, reusing an already-open PR. Verified live with Claude; the PR flow is tested against a bare remote and a stub `gh`. **Not yet done:** setup script, revert to checkpoint |
| Forks, native + cross-provider (M3) | [x] | Thread menu → Fork with Claude Code / Codex. Same harness forks natively (Claude `--resume --fork-session --session-id`; Codex `thread/fork`, falling back to a fresh thread). Another harness gets a deterministic handoff digest (requests, outcome, files changed, open todos, MCP pointer) pre-filled in the composer. Each fork gets its own worktree from the parent's current state. Forks nest under their parent and show a "Forked from" marker. All three paths verified live. **Not yet done:** fork from an arbitrary turn |
| File explorer + viewer | [x] | .gitignore-aware tree; tree-sitter highlighting; Markdown preview; images |
| Subagent view | [x] | Agent cards in the timeline; subagent tab with steps and report |
| Agentloft MCP server | [x] | `browser_*`, `list_threads`, `read_thread`, `get_thread_diff`; verified with Claude and Codex |
| Terminal (M4) | [x] | Ghostty VT (`loft-terminal`, libghostty-vt) on daemon PTYs with an 8 MiB replay; one shell per thread that reattaches after a UI restart; vim, truecolor and CJK verified live. **Not yet done:** tabs, the 50 MB `cat` stress test, URL detection |
| Browser panel | [x] | WKWebView (`loft-browser`, wry) in the side panel; agents drive it over MCP (navigate, snapshot, click, type, eval); Claude verified live |
| Office documents | [x] | `loft-docs`: Word (paragraphs/tables), Excel (sheet grid), PowerPoint (slides), CSV; verified with sample files |
| Queued messages, approval shortcuts (M1) | [x] | ⌘⏎ / ⌘⇧⏎ / ⌘⇧⌫ |
| Image attachments (M1) | [x] | Paste (⌘V), drag-and-drop or the paperclip. The daemon stores images under `~/.agentloft/attachments` (png/jpeg/gif/webp, at most 10 per message, 20 MB each). Claude gets base64 image blocks, Codex gets `localImage`. Timeline thumbnails open on click, and queued messages keep their images. Verified live with both agents |
| Windows and tab groups (M5) | [x] | New Window (⌘⇧N), New Tab (⌘T), and Open in New Tab from the thread menu. Each window has its own daemon connection and selection, so a tab group holds threads from any project. Uses native macOS tabs (`addTabbedWindow:`) with our own tab strip, since GPUI hides AppKit's under the transparent title bar. Window titles follow the thread. Browser calls go to a window that has the thread open. The Dock icon reopens a window. **Not yet done:** restoring windows and tabs on relaunch |
| Settings window (M5) | [x] | ⌘, opens a native window with sections: Appearance (theme, accent swatches, text/code size, density, width, fonts; applied live), Agents (binary, default model and permissions per harness), Worktrees (mode, prefix) and About. Writes `config.toml` with toml_edit, keeping comments. The daemon applies changes via `ReloadConfig`. **Not yet done:** keymap editor, CLI version/auth probe |
| App icon, bundle, About, menus | [x] | `cargo xtask bundle` |

## M1 → M9

See [PLAN §7](PLAN.md#7-milestones). Not started.
