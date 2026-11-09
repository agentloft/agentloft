# ADR-0010: Tokio in the daemon, the GPUI executor in the UI

- **Status:** Proposed
- **Date:** 2026-11-08

## Context
GPUI ships its own async executors: a main-thread foreground executor and a background executor. Many libraries we need (process I/O, sockets, `tokio-util` codecs, ACP's Rust SDK) assume tokio. Zed bridges the two with `gpui_tokio` (Apache-2.0, about 100 lines), which runs a tokio runtime and turns tokio futures into GPUI `Task`s. Vibex vendors a copy of it. Ghostex runs a dedicated tokio runtime and moves data over channels. tty7 avoids tokio by using smol.

## Decision
**Daemon (`agentloftd`)**
- Pure tokio, using the multi-thread runtime.
- One actor task per thread orchestrator.
- SQLite runs on a dedicated blocking writer thread, fed through a channel.
- Harness I/O uses tokio processes with `LinesCodec` / JSON-RPC framing.

**UI (`agentloft`)**
- `loft-client` runs its socket I/O on a small dedicated tokio runtime: 1–2 worker threads, started at app launch.
- It exposes `async-channel` streams and request futures that are runtime-agnostic.
- A long-lived `cx.spawn` loop on the GPUI side drains client events and applies them to GPUI `Entity` stores, batched per frame.
- For one-off awaits from views, a vendored `gpui_tokio`-style helper (with attribution) wraps client calls as GPUI `Task`s.
- Dropping a task cancels the underlying request.

**Rule**
- No blocking I/O on the GPUI main thread.
- Only plain data crosses the boundary: no tokio handles stored in entities and no GPUI types in `loft-client`.

## Consequences
**Benefits**
- Each side uses its natural runtime.
- `loft-client` can be tested without GPUI.
- Batching per frame avoids render storms during streaming.

**Costs**
- Two runtimes in the UI process. Minor.
- We must be careful with cancellation semantics across the bridge.

## Alternatives considered
- **smol everywhere.** Smaller, but loses the tokio ecosystem (ACP SDK, `tokio-util`).
- **Run client I/O on GPUI's background executor.** Possible, but most async I/O crates need a tokio reactor.

## References
- Zed `crates/gpui_tokio`
- Vibex `gpui-tokio` copy
- Ghostex `apps/desktop` channel bridge
