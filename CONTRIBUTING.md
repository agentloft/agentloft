# Contributing to Agentloft

Thanks for helping! Agentloft is early: the plan lives in [`docs/PLAN.md`](docs/PLAN.md), decisions in [`docs/adr/`](docs/adr/README.md), and milestone progress in [`docs/TRACKING.md`](docs/TRACKING.md).

## Getting started

```sh
cargo xtask dev        # build + run the app against an isolated ./.dev-home
cargo xtask ci         # fmt, clippy, tests, cargo-deny (what CI runs)
cargo xtask live-claude  # end-to-end against the real `claude` CLI (costs tokens)
```

- The toolchain is pinned in `rust-toolchain.toml`.
- **Always use an isolated `AGENTLOFT_HOME`** when running dev builds (`xtask dev` does this). Never point a dev daemon at your real `~/.agentloft`.

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
