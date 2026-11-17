# ADR-0012: Multiple windows and user-defined window groups

- **Status:** Accepted
- **Date:** 2026-11-08 (revised 2026-11-09: groups are not tied to projects)

## Context
The product owner wants multiple windows, with groups of windows shown as tabs. **A group is a chat workspace:** it can hold agents (threads) from **different projects**, for example a Claude thread in `api/` next to a Codex thread in `web/`.

GPUI supports multiple windows (`cx.open_window`). Zed `main` also exposes macOS native window tabbing:
- `WindowOptions.tabbing_identifier`: windows that share an identifier merge into one tabbed window.
- `SystemWindowTabController`.

Both were verified in `crates/gpui/src/platform.rs` and `window.rs` on 2026-11-08.

## Decision
- **A tab is one thread**, from any project. The sidebar lists all projects and their threads.
  - Opening a thread adds it as a tab in the current group, or in a new window (⌘⇧O; setting `ui.open_thread_in`).
- **Window groups are first-class and user-defined.** The `window_groups` table stores, per group:
  - `id`
  - `name`
  - ordered `tabs` (thread ids)
  - `active_tab`
  - `frame`
  - `client_key`
- **Group actions:** new, rename, close (threads keep running), reopen recent, and move a tab to another group (menu or drag).
  - On first launch there is one group, "Main".
- **macOS:** each group's windows share `tabbing_identifier = "group:<uuid>"`, so they merge into one native tabbed window. Dragging a tab out creates a new group; dropping it onto another group's window moves it. GPUI's tab callbacks keep the table in sync.
- **Linux and Windows:** an in-app tab strip with the same group model.
- **Shared state:** all windows share one `loft-client` connection and store. Per-thread GPUI entities are shared, so the same thread open in two groups stays in sync while streaming.
- **Restore:** groups, tab order, frames and per-tab UI state (scroll, dock layout) are restored on launch. Remote clients have their own `client_key` and their own groups ([ADR-0014](0014-remote-access-tailscale.md)). Tabs can mix local and remote threads, and each shows an environment badge.

## Consequences
**Benefits**
- Matches how people actually work: one task can span several repos and agents.
- Native macOS feel.
- Consistent state across windows for free.

**Costs**
- We must reconcile native tab drag-and-drop with the `window_groups` table.
- Native tabbing relies on GPUI's macOS implementation, which may have rough edges.
- A thread can appear in several groups at once. That is allowed, because they are views and not owners.

## Alternatives considered
- **One group per project.** Rejected by the product owner, because tasks span projects.
- **Custom in-app tabs everywhere.** Consistent across platforms, but less native on macOS. Kept as the fallback for other platforms.
- **Single window with a Dock.** Doesn't meet the requirement.

## References
- Zed `crates/gpui/src/platform.rs` (`tabbing_identifier`, `SystemWindowTab`)
- Zed `crates/gpui/src/window.rs` (`SystemWindowTabController`)
