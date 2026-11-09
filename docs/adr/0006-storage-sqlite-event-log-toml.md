# ADR-0006: SQLite with CRUD tables plus an event-sourced thread log; TOML config

- **Status:** Proposed
- **Date:** 2026-11-08

## Context
We need durable storage for projects, threads, turns, items, approvals, checkpoints, processes and UI state. Several patterns exist:
- **Full event sourcing** on SQLite: commands go through a decider and produce events, and the events, projections and outbox are written in one transaction. Such designs may prune the log and treat projections as the source of truth.
- **Ghostex** uses CRUD tables on SQLite with many columns holding JSON as text.
- **Every GPUI app we surveyed** uses `rusqlite`.

The daemon is the only process that writes ([ADR-0001](0001-daemon-from-m0.md)). The UI needs reconnect replay, and we want deterministic tests.

## Decision
**Engine**
- SQLite via `rusqlite` with the `bundled` feature, in a single `agentloft.db` under `~/.agentloft/`.
- Pragmas: WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`.

**Data model: a hybrid**
- **CRUD** for projects, settings, UI state, processes, terminals and drafts.
- **Event-sourced thread timelines:**
  - `thread_events` is an append-only table, **kept forever**.
  - Projections (`turns`, `items`, `runtime_requests`) are updated in the same transaction and can be rebuilt from the log.
  - Command receipts provide idempotency.
- **Deltas** are not stored row by row. They are broadcast live, and the item snapshot is written about every 250 ms and when the item completes.

**Migrations**
- Embedded and forward-only (`rusqlite_migration`).
- Each migration is preceded by a `VACUUM INTO backups/…` copy; the last 5 are kept.

**Config**
- User config lives in `~/.agentloft/config.toml`; key bindings in `keymap.toml`.
- Each project can add `<repo>/.agentloft/config.toml`.
- Edits made from the UI use `toml_edit`, so comments are preserved.
- Data home layout and `AGENTLOFT_HOME` override: see PLAN §4.1.

## Consequences
**Benefits**
- One embedded database file, no server, and fast.
- The log provides reconnect replay (`after_seq`), audit and debugging, generation of fork and handoff context, and decider-level tests.
- TOML is hand-editable and familiar to Rust and Codex users.

**Costs**
- Two write paths: CRUD and the event log.
- Projection rebuild code must be maintained.
- The log grows without bound. It is small relative to transcripts, and archived threads can be compacted later if needed.

**Risk**
- A crash can lose up to about 250 ms of streamed text. This is acceptable, because the native session still holds the full content.

## Alternatives considered
- **Full event sourcing for everything**. Adds ceremony for settings and projects with no benefit.
- **Pure CRUD.** Loses replay and idempotency.
- **`sqlx`.** Async, but compile-time query checking needs extra setup, and the daemon serializes writes on one writer thread anyway.
- **`redb` / `sled`.** No SQL and no FTS5 search.
- **JSON or YAML config.** TOML is the Rust-ecosystem norm, and Codex uses it.

## References
- Ghostex `server/src/storage/`
