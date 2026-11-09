<p align="center">
  <img src="assets/icon/png/agentloft-128.png" width="112" height="112" alt="Agentloft icon">
</p>

<h1 align="center">Agentloft</h1>

A native desktop app for running coding agents side by side: **Claude Code**, **OpenAI Codex**, and ACP agents such as Gemini CLI and OpenCode. It is built in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

> **Status:** planning. No app code yet.
> - Start with the [architecture and delivery plan](docs/PLAN.md).
> - The design decisions are in the [ADRs](docs/adr/README.md).

## What it will do

- **Projects and threads.** Each thread can run in its own git worktree, with a checkpoint every turn. That gives you turn and cumulative diffs, revert, and merge-back.
- **Native protocols.** Agents are driven through their structured protocols (Codex `app-server`, Claude stream-json, ACP), never by scraping their terminal UI. You get exact streaming, approvals, interrupts, resume and forks.
- **Agents keep running.** A headless daemon (`agentloftd`) owns the agents, so they continue when the window closes or updates.
- **Window groups.** Several windows, grouped into native tabs, mixing agents from different projects.
- **Cross-thread context.** Agents can read other threads through a built-in MCP server.
- **Terminal and remote access.** An integrated terminal, plus optional remote access over your own Tailscale tailnet or SSH.

macOS (Apple Silicon) comes first; Linux and Windows follow.

## Building

```sh
cargo xtask dev            # run the app against an isolated ./.dev-home
cargo xtask bundle --open  # build target/bundle/Agentloft.app (macOS)
```

## License

[Apache-2.0](LICENSE)
