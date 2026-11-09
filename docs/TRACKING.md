# Milestone tracking

Local, checked-in tracker for [PLAN §7](PLAN.md#7-milestones). Update it in the same PR as the work.
Legend: `[x]` done and verified · `[~]` partial · `[ ]` not started.

## M0: Walking skeleton (in progress, branch `m0/scaffold`)

| # | Acceptance criterion | Status | Evidence |
|---|---|---|---|
| 1 | `cargo xtask dev` builds both binaries and runs against `./.dev-home` | [x] | `xtask/src/main.rs` |
| 2 | Add project (folder picker) and new thread; both survive relaunch | [x] | Folder picker in sidebar; persistence covered by `loftd/tests/e2e.rs` |
| 3 | Multi-line composer: Enter sends, Shift+Enter newline; Japanese IME commits correctly | [~] | Enter and Shift+Enter work; **IME test still manual and pending** |
| 4 | Token-by-token streaming markdown; tool rows with status; cost and usage shown | [x] | Live screenshot 2026-11-09; header shows the last turn's cost and tokens |
| 5 | Claude default mode: approval card, Allow runs the tool, Deny is handled | [x] | `live_claude` test, plus a UI screenshot of the approval card |
| 6 | Stop interrupts within 2 s | [x] | `live_claude`: interrupt took 14 ms |
| 7 | Quitting the UI mid-turn keeps the turn running; reopening catches up | [~] | Daemon independence is covered by e2e (reconnect gets a full snapshot); **manual quit-mid-turn check pending** |
| 8 | `kill -9` of the daemon: UI respawns it, turn shows interrupted, the next message resumes the same session | [~] | Recovery + resume verified by `live_claude` (restart) and `loft-core` tests; UI auto-respawns on disconnect; **manual kill -9 check pending** |
| 9 | No orphan `claude` processes after shutdown; reaper | [~] | Process groups + graceful stop + startup reaper implemented; **no automated reaper test yet** |
| 10 | Tests: decider/core, protocol round-trip, Claude fixtures (2.1.319), headless e2e | [x] | 27 tests across crates (plus the ignored `live_claude`); fixtures in `crates/loft-harness-claude/fixtures/` |

Infrastructure:
- [x] CI: macOS (fmt, clippy, tests), Linux headless tests, cargo-deny
- [x] `deny.toml` dependency policy (ADR-0009)
- [x] CONTRIBUTING (DCO), PR and issue templates
- [ ] Branch protection on `main` (after the first CI run is green)

## M0.5: Design pass — "professional UI" (next)

Goal: visual quality on par with Codex and Claude Desktop, plus appearance customization, without regressing performance.

- [ ] Design tokens: type scale, spacing, radii, elevation; light and dark themes following the system
- [ ] Appearance settings in `config.toml` `[ui]`: theme, accent color, UI and mono fonts, font size, density; live reload
- [ ] Integrated title bar (transparent, traffic lights inset), breadcrumb header
- [ ] Sidebar: search, project groups, thread rows with relative time, running spinner and needs-approval badge
- [ ] Conversation column: centered max width, assistant prose without boxes, user bubbles, grouped tool activity ("Ran 3 commands") that expands on click
- [ ] Floating composer card: model and mode chips, round send/stop button, attachment affordance
- [ ] Approval card styled as a first-class prompt with keyboard shortcuts
- [ ] Empty states and onboarding (no projects, no threads, daemon connecting)
- [ ] Performance: 10k-item thread at ≥ 60 fps while streaming; no per-row theme clones

## M1 → M9

See [PLAN §7](PLAN.md#7-milestones). Not started.
