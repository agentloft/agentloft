# ADR-0001: Headless core as a daemon (`agentloftd`) from M0

- **Status:** Accepted
- **Date:** 2026-11-08

## Context
Agentloft drives long-running coding agents. A single turn can run for tens of minutes. Two architectures were considered:
- **The core runs in-process**, inside the GPUI app.
- **The core runs as a separate process.** Ghostex does it with the `gxserver` daemon plus per-session `zmx` holders.

The product owner wants agents to keep running when the UI quits, crashes, or updates. They also want everything in Rust and as fast as possible.

## Decision
- The core is a separate binary, **`agentloftd`**, built on tokio with no GPUI dependency. It is used from **M0**.
- It owns:
  - SQLite;
  - all agent CLI processes;
  - PTYs;
  - git operations.
- The GPUI app **`agentloft`** is a client. It talks to the daemon over a Unix domain socket at `~/.agentloft/run/agentloftd.sock`:
  - The socket's directory is mode 0700, and the peer UID is checked.
  - Messages are length-delimited JSON frames.
  - The protocol is versioned through a `Hello`/`Welcome` handshake.
  - It carries commands, queries, subscriptions with `after_seq` replay, and ephemeral deltas.
- **Lifecycle:**
  - During M0–M6 the UI spawns the daemon on demand, detached.
  - From M7 the daemon is a LaunchAgent registered via `SMAppService`, with `KeepAlive`.
  - A `flock`-based lock file guarantees one daemon per `AGENTLOFT_HOME`.
- **Updates:**

  | Case | Behaviour |
  |---|---|
  | UI-only update | Never touches the daemon. |
  | Daemon update, daemon idle | The daemon takes a DB backup, closes its harnesses gracefully, exits, and the new binary starts. This takes about 1–2 s. |
  | Daemon update, daemon busy | The update waits until the daemon is idle. The UI shows a banner with "Restart now". |
  | Forced restart | In-flight turns are interrupted. Conversations resume through their native session IDs. |

  - The protocol supports versions N and N−1.
  - Database migrations are forward-only, and the DB is backed up before each one.
- **Session holders (optional, M9):** a small, stable `agentloft-hold` process per agent that keeps the agent's stdio open while the daemon restarts.

## Consequences
**Benefits**
- Agents survive UI quits, crashes and updates. UI code changes the most, so most updates are UI-only.
- The boundary between the headless core and the UI is enforced by the process split, not by convention.
- Tests can drive the real daemon through `loft-client` without a GPU.
- Remote and mobile clients can be added later without re-architecting.
- Terminals survive UI restarts.

**Costs**
- An IPC protocol to design and version.
- Daemon upgrade orchestration.
- Two processes to debug and log.
- About 1–2 weeks of extra work before M0.
- Developers must always use an isolated `AGENTLOFT_HOME` (`cargo xtask dev`). Ghostex learned this the hard way: a dev daemon killed real sessions.

**Neutral**
- Local Unix-socket IPC adds microseconds of latency, which is negligible next to model token latency.

## Alternatives considered
- **In-process core, daemon later (M7).** Simpler at first, but quitting or updating the app interrupts turns. Rejected by the product owner.
- **Daemon plus session holders from M0.** The most durable option, but the most work. Deferred to M9.
- **gRPC or HTTP+WebSocket transport.** Heavier than needed for local use. An authenticated WebSocket can be added for remote clients later.

## References
- Ghostex `AGENTS.md`, `server/src/zmx/launch.rs`, `server/src/session_self_heal.rs`
