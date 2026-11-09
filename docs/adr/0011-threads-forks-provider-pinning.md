# ADR-0011: Threads pinned to a provider; forks to any provider

- **Status:** Accepted
- **Date:** 2026-11-08

## Context
Switching provider mid-thread needs an explicit context-handoff artifact. That complicates the domain model: one app thread maps to many provider threads. The product owner decided that **a thread can't switch providers but can fork**, and that a fork may target **any** provider.

Native fork support:
- **Claude:** `--resume <id> --fork-session`, optionally with `--resume-session-at <message>`.
- **Codex:** `thread/fork{threadId, lastTurnId, cwd}`.
- **ACP:** `session/fork`, unstable.

## Decision
**Pinning**
- `Thread.harness_id` is immutable. Model, effort and permission mode may change within the harness.

**Fork**
- *Fork from turn N* creates a new thread with `parent{thread_id, turn_id, kind}`.

**Same provider**
- Uses a **native fork** at turn N where the provider supports it (`kind = native`).
- Otherwise falls back to a handoff.

**Different provider**
- Uses a **context handoff** (`kind = handoff`).
- A first message is generated from the parent's event log. It contains:
  - user goals and requests;
  - the assistant's final answers;
  - the files changed;
  - the cumulative diff summary;
  - open TODOs and plan items.
- It is trimmed to a token budget.
- The user can review and edit it before sending.
- The digest is **deterministic** (decided): instant, free and reproducible.
- It ends with the parent's thread id. The new agent can read the parent's full history on demand through the Agentloft MCP server ([ADR-0013](0013-agentloft-mcp-cross-thread-context.md)). This replaces "ask the source agent to summarise".

**Workspace**
- By default the fork gets a **new worktree branched from the parent's checkpoint commit at turn N**, so code state and conversation state match ([ADR-0008](0008-git-worktrees-checkpoints.md)).
- Option: share the parent's workspace.

**UI**
- Forks nest under their parent in the sidebar. A fork marker item appears in both timelines.

## Consequences
**Benefits**
- One provider session per thread, which keeps the domain simple.
- The trade-off between native and handoff fidelity is explicit.
- Agents can be compared side by side by forking the same point to Claude and Codex.

**Costs**
- Handoff fidelity is lossy by nature: no tool-call internals and no provider caches.
- Forks are possible only when the workspace is a git repo, because they rely on checkpoints. In non-git projects, forks share the workspace.

## Alternatives considered
- **Mid-thread provider switching.** Rejected by the product owner: it adds complexity and makes the transcript ambiguous.
- **Same-provider forks only.** Loses the compare-agents workflow.

## References
- Codex `ThreadForkParams` schema (codex-cli 0.167.0)
