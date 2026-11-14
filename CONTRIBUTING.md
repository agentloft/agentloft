# Contributing to Agentloft

Thanks for helping! Agentloft is early: the plan lives in [`docs/PLAN.md`](docs/PLAN.md), decisions in [`docs/adr/`](docs/adr/README.md), and milestone progress in [`docs/TRACKING.md`](docs/TRACKING.md).

## Getting started

```sh
cargo xtask dev        # build + run the app against an isolated ./.dev-home
cargo xtask ci         # fmt, clippy, tests, cargo-deny (what CI runs)
cargo xtask live-claude  # end-to-end against the real `claude` CLI (costs tokens)
```

- The toolchain is pinned in `rust-toolchain.toml`.
- `cargo xtask ci` needs `cargo install cargo-hakari cargo-deny`.
- **The web client is built with the app.** `cargo xtask dev`, `bundle` and `install` also build the browser version of the UI into `target/web` (`install` and `bundle` in release), which the daemon serves. It is the same `loft-ui` code built for wasm, so a desktop build without it leaves browsers on an old UI that lacks the latest changes. `--no-web` skips it when you only need the desktop app.
- **Always use an isolated `AGENTLOFT_HOME`** when running dev builds (`xtask dev` does this). Never point a dev daemon at your real `~/.agentloft`.

## Keeping builds fast

A build from nothing takes about a minute; after that, a change should cost seconds whatever cargo command you run. Three things keep it so:

- **One build of each dependency.** Cargo picks a dependency's features from the packages a command names, so `-p loft-ui`, `-p loft-app -p loftd` and `--workspace` used to compile GPUI and everything on it once each. Every crate depends on `crates/workspace-hack`, which names those dependencies with all their features ([`.config/hakari.toml`](.config/hakari.toml)). After changing a dependency or its features, run `cargo hakari generate`; CI fails until you do.
- **The terminal's view tests are opt-in**: `cargo test -p loft-terminal --features test-support` (CI and `cargo xtask ci` run them). They need GPUI's test platform, and switching that on for every test run compiled GPUI a second time.
- **libghostty-vt is built once per machine**, not once per checkout and profile ([`vendor/ghostty-vt-sys`](vendor/ghostty-vt-sys/README.md)): a new worktree copies it from `~/Library/Caches/ghostty-vt-sys`.

If a command you expect to be quick recompiles dependencies, something has split the build again: `cargo build --timings` shows what was compiled.

## Architecture rules

- `loft-core`, `loft-store`, `loft-harness*` and `loftd` must not depend on GPUI.
- `loft-ui` / `loft-app` never spawn processes or touch SQLite; everything goes through `agentloftd`.
- Agents are driven through structured protocols only — no TUI scraping (ADR-0002).
- New harness behaviour needs fixture coverage (recorded CLI transcripts under `crates/loft-harness-*/fixtures/`).

## Licensing and sign-off

Agentloft is licensed under [Apache-2.0](LICENSE). Contributions are accepted under the same license (inbound = outbound).

Please sign off every commit with the [Developer Certificate of Origin](https://developercertificate.org/):

```sh
git commit -s -m "Your message"
```

Do not copy code from GPL/AGPL projects (for example Zed's `editor`, `terminal`, `markdown`, `ui` crates, or Vibex). `cargo deny check` enforces the dependency license policy (ADR-0009).

## Pull requests

- Keep PRs focused; describe what changed, how you verified it, and what remains untested.
- CI must be green: format, clippy with `-D warnings`, tests on macOS, headless tests on Linux, and `cargo deny`.
