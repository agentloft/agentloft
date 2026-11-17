# ADR-0004: Codex via `app-server` JSON-RPC with schema-generated types

- **Status:** Proposed
- **Date:** 2026-11-08

## Context
`codex app-server` is the protocol OpenAI's own IDE extension and desktop app use. It is JSON-RPC 2.0 over stdio as JSONL, with the `jsonrpc` field omitted. It is still marked "[experimental]" but it is feature-rich:
- threads: start, resume, fork, revert;
- turns: start, steer, interrupt;
- typed approval requests;
- granular item notifications;
- `model/list`.

The CLI can emit its own schema with `codex app-server generate-json-schema --out DIR` and `generate-ts`.

OpenAI's Rust crate `codex-app-server-protocol` is Apache-2.0, but OpenAI does not publish it: it has version `0.0.0` and is available only as a git dependency, which pulls in many internal crates. A client can instead be generated from the committed JSON schema at a pinned upstream ref.

## Decision
**Process**
- Spawn our own `codex app-server` child, one per Codex instance (keyed by `CODEX_HOME`), shared across threads.
- `process_mode = "per-thread"` is available in config as a fallback.
- Never attach to the managed `codex app-server daemon`.

**Types**
- `cargo xtask schema-codex` runs `generate-json-schema` from the installed `codex`, writes the output to `crates/loft-harness-codex/schema/<codex-version>/`, and generates serde types with `typify`.
- The generated Rust is checked in.

**Protocol use**
- Handshake: send `initialize` with `experimentalApi`, then the `initialized` notification.
- Use `turn/completed` as the authoritative end of a turn. `turn/interrupt` resolves before the interrupted status arrives.
- Approvals map all decision variants: `accept`, `acceptForSession`, `acceptWithExecpolicyAmendment`, `applyNetworkPolicyAmendment`, `decline` and `cancel`. The legacy `execCommandApproval` and `applyPatchApproval` requests are also handled.
- Permission mode and sandbox are passed per turn only when the user chose them. Otherwise `~/.codex/config.toml` applies.

## Consequences
**Benefits**
- Precise types and a small dependency footprint.
- Upgrades are mechanical: regenerate the types, then review the diff.

**Costs**
- The protocol moves fast. In codex-cli 0.167 there are about 110 client methods and 90 notification types.
- We must re-run codegen and update the mappings.
- The nightly canary diffs the schema of the latest Codex against the committed one.

**Risk**
- The schema-driven parsers can break if Codex changes fields we depend on. Mitigations:
  - unknown notifications are ignored;
  - unknown item types become an `Unknown` item;
  - a version range is checked in `probe()`.

## Alternatives considered
- **Git dependency on `openai/codex` `codex-app-server-protocol`.** Exact types, but a heavy build, unpinnable churn, and a coupling to internal crates.
- **`codex exec --json`.** Not interactive: no approvals, steering or resume control.
- **`codex-acp` through ACP.** Loses execpolicy amendments, steer and revert.

## References
- `openai/codex` `codex-rs/app-server/README.md` and `codex-rs/app-server-protocol/schema/`
- Schema generated locally from codex-cli 0.167.0
