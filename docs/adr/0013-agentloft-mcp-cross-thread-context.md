# ADR-0013: Agents read other threads through a built-in Agentloft MCP server

- **Status:** Accepted
- **Date:** 2026-11-09

## Context
The product owner wants agents to be able to **read other threads**. Typical requests:
- "check what the Codex thread found";
- "continue from the plan in thread X";
- pulling exact details after a cross-provider handoff ([ADR-0011](0011-threads-forks-provider-pinning.md)).

All of our harnesses support MCP servers:
- Claude: `--mcp-config`.
- Codex: `mcp_servers.*` config, set with `-c` overrides or the per-thread `config` in `thread/start`.
- ACP: `session/new{mcpServers}`.

So MCP is the one mechanism that works for every provider without prompt-stuffing.

## Decision
**The server**
- `agentloftd mcp --thread <id> --token <capability>` is a stdio MCP server: the same binary, in a different mode.
- It connects to the daemon socket as `client_kind = mcp-bridge`, with a per-session capability token tied to one thread and its allowed scope.
- It is attached to every harness session as the server named `agentloft`.

**v1 tools (read-only)**

| Tool | Purpose |
|---|---|
| `list_threads` | List threads in scope |
| `read_thread{thread_id, from_turn?, to_turn?, detail}` | Read a transcript; `detail` is `summary`, `messages` or `full` |
| `search_threads` | FTS5 search across threads |
| `get_thread_diff` | Checkpoint diffs for a thread |
| `get_thread_plan` | A thread's latest plan and todo items |

Responses are paginated and size-capped.

**Scope**
- Setting `context.cross_thread_scope`:
  - `project` (default): the same project, plus the thread's own fork parent and children;
  - `all`;
  - `off`.
- There is also a per-thread toggle.
- Archived threads are excluded unless explicitly requested.

**Visibility**
- Tool calls render in the timeline as `mcp_tool_call` items, and harness approval rules apply as usual.

**Handoffs**
- Cross-provider handoff digests end with a pointer to the parent thread's id, so the new agent can read the parent on demand.

**Out of scope for v1**
- Write tools (`post_to_thread`, `spawn_thread`) are deferred to a future ADR, and would be gated by approvals.

## Consequences
**Benefits**
- One implementation covers Claude, Codex and every ACP agent.
- Agents fetch only what they need, which is cheaper than pasting transcripts.
- Users can see what was read.
- It is a foundation for agent-to-agent workflows later.

**Costs**
- An extra lightweight process per active agent session (the stdio bridge).
- A token-scoped auth path in the daemon.
- The tool schemas must stay stable.

**Risks**
- Context leakage between unrelated work. Mitigated by the same-project default, the `off` setting and visible tool calls.
- Agents over-reading large threads. Mitigated by size caps, pagination and summary-first detail levels.

## Alternatives considered
- **Pasting other threads into prompts.** Expensive and stale, with no on-demand access.
- **Exposing the thread store as files**, for example a `.agentloft/threads/*.md` export. Leaks into repos, stale, with no scoping.
- **Provider-specific mechanisms.** Different for each harness, and ACP has none.

## References
- PLAN §2.9
- Claude `--mcp-config`
- Codex `mcp_servers` config
- ACP `session/new` `mcpServers`
