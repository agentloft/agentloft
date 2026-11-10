# ADR-0015: Agents orchestrate threads through the Agentloft MCP server

- **Status:** Accepted
- **Date:** 2026-11-09

## Context
ADR-0013 gave agents read-only access to other threads and deferred write tools (`spawn_thread`, `post_to_thread`). The product owner wants agents to **orchestrate threads**: start other agents (any installed agent, model, reasoning effort and permission mode), let them work in parallel, and collect their results. A spawned agent should use the current thread's settings unless the user explicitly asks for something else.

## Decision
**Tools** on the `agentloft` MCP server, which already runs attached to every agent session:

| Tool | What it does |
|---|---|
| `list_agents` | Installed agents with their models and efforts, the permission modes, and the calling thread's own settings (the defaults) |
| `spawn_thread{prompt, title?, agent?, model?, effort?, permission_mode?, worktree?, top_level?, project?, chat?}` | Start a new thread and send it `prompt`; returns its id at once. With `top_level`, it sits on its own in the sidebar (`ThreadParent.detached`) but stays linked to its caller. `project` (name, path or id) starts it in another project's folder (or a new worktree there); `chat` starts it as a chat |
| `wait_for_threads{thread_ids, timeout_seconds?}` | Wait until the threads stop running (done, failed or waiting for approval); returns each status and latest reply. The default timeout is 50 s, which stays under Codex's MCP tool timeout, so agents call again while threads are still running |
| `thread_status{thread_id}` | One thread's status, settings and latest reply |
| `list_projects` / `add_project{path, name?}` | The projects (id, name, path, thread count, Chats, the caller's `current`), and adding a folder as one; `list_threads{project?}` filters by project |
| `send_to_thread{thread_id, message}` | Follow up with a thread this thread spawned |
| `stop_thread{thread_id}` | Stop a thread this thread spawned |

**Core**
- `Request::SpawnThread` creates a thread linked to its caller (`ThreadParent{kind: Spawned}`) and sends it the prompt.
- Defaults:
  - The agent is the caller's.
  - The same agent keeps the caller's model and effort. Another agent gets its own configured defaults.
  - Permissions are inherited.
  - The spawned thread shares the caller's checkout. `worktree: true` gives it a worktree from the caller's current state instead.
- `Request::ThreadOutcome` reports where a thread stands.

**Limits**
- Spawned threads nest at most 3 deep.
- At most 8 of a thread's spawned threads may be running at once.
- Only threads the caller spawned can be messaged or stopped. Any thread can be read or awaited.

**UI**
- Spawned threads sit under their parent in the sidebar, marked ↳.
- The `spawn_thread` call renders in the parent's timeline as a live agent card: agent logo, title, status and model. Click it to open the thread; ⌘-click opens it beside.
- Wait, message and stop calls name the threads they touch.
- The thread menu has "Copy Thread ID", for pointing an agent at a thread by hand.

## Consequences
**Benefits**
- One mechanism works for every agent. Spawned work is ordinary threads: visible, resumable and forkable.
- The user can step into any spawned thread, approve its requests, or take it over.

**Costs**
- More tools in every session's tool list.

**Risks**
- Runaway fan-out. Mitigated by the depth and concurrency limits and by visible threads.
- Parallel edits colliding in a shared checkout. Mitigated by the `worktree` option, which the tool description recommends for parallel editing.

## Alternatives considered
- **Provider subagents only** (Claude's Task tool). These are one provider, hidden from the sidebar, and can't be resumed.
- **A separate orchestration daemon API.** The MCP server already authenticates per thread and reaches every agent.
