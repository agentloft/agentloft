# ADR-0007: Daemon-owned PTYs, `alacritty_terminal` rendering in GPUI

- **Status:** Accepted, amended 2026-11-09: the UI parser is **libghostty-vt** (see below)
- **Date:** 2026-11-08

## Context
We need an integrated terminal that survives UI restarts, because the daemon owns processes ([ADR-0001](0001-daemon-from-m0.md)). The options for parsing terminal output into a screen state are:

| Option | License | Notes |
|---|---|---|
| `alacritty_terminal` 0.26 | Apache-2.0 | Pure Rust. Used by Vibex, tty7 (fork) and `gpui-terminal`. |
| `libghostty-vt` | MIT | Used by Ghostex via a vendored Ghostty build. Excellent fidelity, but needs **Zig** at build time. |
| `wezterm-term` | — | Not published on crates.io. |
| Zed's `terminal` / `terminal_view` | **GPL-3.0** | Excluded by [ADR-0009](0009-license-and-dependency-policy.md). |

For PTYs, `portable-pty` 0.9 (MIT, from wezterm) covers both openpty and ConPTY.

## Decision
**Daemon side (`loft-pty`)**
- Spawns shells with `portable-pty` in their own session or process group.
- Runs one reader task per PTY.
- Keeps an 8 MB raw-output ring buffer per terminal.
- Broadcasts `TerminalOutput{id, bytes}` to subscribers and accepts input and resize commands.

**UI side (`loft-ui::terminal`)**
- Feeds the bytes into an `alacritty_terminal::Term` and renders it with a custom GPUI `Element`, using structural patterns from Ghostex's MIT `terminal_element.rs`:
  - a shaped-row cache invalidated by damage;
  - fixed cell width;
  - wide-char runs;
  - about 4 ms wakeup coalescing.
- Handles input encoding (keys, mouse, bracketed paste), IME via GPUI's `InputHandler`, selection, links and search.
- On reconnect, replays the ring buffer into a fresh `Term`.

**Swappability**
- The parser sits behind a `TerminalBackend` trait, so `libghostty-vt` can replace `alacritty_terminal` later without touching the daemon side.

## Amendment (2026-11-09): libghostty-vt, not `alacritty_terminal`

The product owner asked for a Ghostty terminal, so `loft-terminal` uses libghostty-vt through the
`ghostty-vt` 0.8.0 crate (Apache-2.0 bindings, Ghostty 1.4.0 at a pinned sha256, built with **Zig 0.17**).
`libghostty-vt` 0.2.2 was not usable: it pins a Ghostty commit that needs Zig 0.15.

What stays as decided above:
- Daemon-owned PTYs with an 8 MiB ring buffer.
- The `TerminalBackend` trait, so `alacritty_terminal` remains a drop-in fallback.
- The custom GPUI element.

What changed:
- **Builds need Zig.** Contributors and CI need Zig 0.17; CI uses `mlugg/setup-zig`.
- **The bus carries no bytes.** Output notifications are offsets only, and subscribers read new bytes from the ring.
- **Terminals reattach after a UI restart.** Each is keyed (`thread:<id>`, `thread:<id>#n` for tabs) so the UI rebuilds its tabs.
- **Rendering approximations.** Double, dotted and dashed underlines draw as plain lines, and box-drawing lines are 1 px.
- **Credits.** Ghostex patterns and Ghostty's license are credited in `THIRD_PARTY_NOTICES.md`.

## Consequences
**Benefits**
- Pure Rust and no Zig.
- A proven crate.
- Terminals survive UI restarts.
- The daemon stays render-agnostic, so future clients can render with xterm.js or similar.

**Costs**
- Bytes are parsed only in the UI. The daemon doesn't know screen contents, which is fine because we never scrape TUIs ([ADR-0002](0002-structured-protocols-not-tui-scraping.md)).
- Replaying an 8 MB ring buffer on reconnect takes about 100 ms. That is acceptable.
- The custom element is real work, roughly 2–4k lines.
- Terminals do not survive *daemon* restarts until session holders exist (M9).

## Alternatives considered
- **`libghostty-vt` now.** Better fidelity (kitty keyboard protocol, graphics), but the Zig toolchain dependency and FFI surface cost more than they're worth for v1.
- **`gpui-terminal` crate.** Targets the stale `gpui` 0.2.2. Reference only.
- **Parsing in the daemon and shipping cell grids.** More bandwidth and complexity. Not needed.

## References
- Ghostex `apps/desktop/src/terminal_element.rs`, `terminal_model.rs`, `src/ghostty_vt/`
- https://crates.io/crates/alacritty_terminal
