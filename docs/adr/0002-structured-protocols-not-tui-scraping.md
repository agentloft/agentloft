# ADR-0002: Structured agent protocols, never TUI scraping

- **Status:** Accepted
- **Date:** 2026-11-08

## Context
There are two ways to drive agent CLIs from a GUI:

1. **TUI scraping.** Run the interactive TUI in a PTY, type prompts as keystrokes, parse the screen for permission panels, send Esc to interrupt, type `/model`, and tail the agent's transcript files. Ghostex does this for 20+ agents. The result is 200+ `session_chat_*` heuristic files that are tied to specific CLI versions, plus documented "self-heal ladders" for stuck sends.
2. **Structured protocols.** Talk to the agent over a programmatic interface:
   - `codex app-server` (JSON-RPC);
   - `claude -p --input-format/--output-format stream-json` with its control protocol;
   - ACP (Agent Client Protocol), which Gemini, OpenCode, Cursor, Copilot, Goose and others speak natively, with adapters available for Claude and Codex.

## Decision
- Integrate every harness through a **structured protocol**.
- **Claude Code and Codex** use their native protocols ([ADR-0003](0003-claude-stream-json-in-rust.md), [ADR-0004](0004-codex-app-server-generated-types.md)).
- **Other agents** use ACP via the `agent-client-protocol` crate (stable v1 schema).
- An agent with no structured interface is **not supported as a harness**. The user can still run it in the integrated terminal.

## Consequences
**Benefits**
- Exact streaming, typed approvals, reliable interrupts, and resume by ID.
- No dependence on screen layout. CLI UI changes don't break us.
- Testable with recorded fixtures.

**Costs**
- Some agents have no structured mode and are only usable in the terminal.
- ACP loses some native richness: Codex execpolicy amendments, steering, revert and fork. That is why Claude and Codex stay native.

**Risks**
- Native protocols drift too. Mitigated by tolerant parsing, per-version fixtures and a nightly canary (PLAN §2.10).

## Alternatives considered
- **ACP for everything, including Claude and Codex via adapters.** One code path, but it adds an extra translation process (Node adapters) and loses features. Rejected for the two primary harnesses.
- **TUI scraping.** High maintenance and fragile, as Ghostex shows. Rejected.

## References
- Ghostex `ai/chat-send-debugging.md`, `server/src/session_chat_*`
- https://agentclientprotocol.com
