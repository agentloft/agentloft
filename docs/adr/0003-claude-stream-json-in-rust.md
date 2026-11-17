# ADR-0003: Claude Code via the stream-json control protocol, in Rust

- **Status:** Accepted
- **Date:** 2026-11-08

## Context
There are two ways to integrate Claude Code programmatically:

1. **The official Agent SDK.** It exists for TypeScript and Python (MIT). Internally, the SDK spawns `claude` with `--output-format stream-json --input-format stream-json --verbose --permission-prompt-tool stdio` and speaks a bidirectional `control_request`/`control_response` protocol over stdio.
2. **Speak that same protocol directly** from our own code.

There is no official Rust SDK. Community crates exist: `claude-codes`, which tracks CLI versions, and `claude-agent-sdk-rs`. The product owner wants a pure-Rust stack.

## Decision
Implement the protocol directly in `loft-harness-claude`:

**Process**
- One `claude -p` process per active thread. The process stays alive across turns. It closes after an idle timeout and is reopened with `--resume`.
- We pre-assign the session ID with `--session-id <uuid>`.
- Partial-message deltas are enabled with `--include-partial-messages`. Our own user messages are echoed back with `--replay-user-messages`.

**Message flow**
- **CLI to us:** `can_use_tool` approval requests, answered with an allow or deny reply. Allow may carry `updatedPermissions`; deny may set `interrupt`.
- **Us to CLI:** the control requests `interrupt`, `set_model`, `set_permission_mode`, `rewind_files` and `get_context_usage`.
- **Images:** sent as base64 content blocks.

**Defaults**
- `--permission-mode` is passed only when the user picked one. Otherwise Claude's own default applies.

**Types**
- We write our own serde types and make them tolerant: unknown message types become an `Unknown{raw}` item instead of an error.
- `claude-codes` serves as a reference and test oracle, after we check its license.

## Consequences
**Benefits**
- No Node or Bun runtime to ship or locate.
- One fewer process per thread.
- Full control over backpressure and lifecycle.
- Fast startup.

**Costs**
- The control protocol has **no public spec**. We follow the SDK sources, and new SDK features need porting by hand.

**Mitigations**
- Fixtures recorded per CLI version and replayed in CI.
- `probe()` reports a known-good version range.
- A nightly canary runs against the latest CLI.
- A designed but unbuilt fallback: `loft-harness-claude-sdk`, the official TS SDK running under Bun behind the same `Harness` trait. We build it only if the raw protocol becomes unworkable.

## Alternatives considered
- **Node or Bun sidecar with the official SDK.** The safest choice against drift, but it breaks the pure-Rust and minimal-process goals. Kept as a fallback.
- **The `claude-agent-acp` adapter through ACP.** Adds a Node process and loses fidelity.
- **The community Rust SDK crates as dependencies.** Each one is a single-maintainer risk. They are useful as references.

## References
- https://code.claude.com/docs/en/headless
- `anthropics/claude-agent-sdk-python` `src/claude_agent_sdk/_internal/query.py` and `types.py`
- Local `claude --help` (v2.1.319)
