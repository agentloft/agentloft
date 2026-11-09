# Architecture Decision Records

Each ADR records one significant decision: its context, the decision, the consequences, and the alternatives considered. Status is **Proposed**, **Accepted**, or **Superseded by NNNN**. "Accepted" means the product owner has decided it. "Proposed" means it is the recommended default and is open for review.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-daemon-from-m0.md) | Headless core as a daemon (`agentloftd`) from M0 | Accepted |
| [0002](0002-structured-protocols-not-tui-scraping.md) | Structured agent protocols, never TUI scraping | Accepted |
| [0003](0003-claude-stream-json-in-rust.md) | Claude Code via the stream-json control protocol, in Rust | Accepted |
| [0004](0004-codex-app-server-generated-types.md) | Codex via `app-server` JSON-RPC with schema-generated types | Proposed |
| [0005](0005-gpui-pinning-and-staying-current.md) | GPUI pinning via gpui-kit's `gpui-pre`, and staying current | Proposed |
| [0006](0006-storage-sqlite-event-log-toml.md) | SQLite with CRUD tables plus an event-sourced thread log; TOML config | Proposed |
| [0007](0007-terminal-architecture.md) | Daemon-owned PTYs, `alacritty_terminal` rendering in GPUI | Proposed |
| [0008](0008-git-worktrees-checkpoints.md) | Git CLI, per-thread worktrees, hidden-ref checkpoints | Proposed |
| [0009](0009-license-and-dependency-policy.md) | Apache-2.0 and a no-copyleft dependency policy | Accepted |
| [0010](0010-async-runtimes.md) | Tokio in the daemon, the GPUI executor in the UI | Proposed |
| [0011](0011-threads-forks-provider-pinning.md) | Threads pinned to a provider; forks to any provider | Accepted |
| [0012](0012-multi-window-and-window-groups.md) | Multiple windows and user-defined window groups | Accepted |
| [0013](0013-agentloft-mcp-cross-thread-context.md) | Agents read other threads through a built-in Agentloft MCP server | Accepted |
| [0014](0014-remote-access-tailscale.md) | Remote access over the user's own Tailscale tailnet, or SSH | Accepted |

To add an ADR, copy the structure of an existing one, use the next number, and add a row to this table.
