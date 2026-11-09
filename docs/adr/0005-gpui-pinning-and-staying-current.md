# ADR-0005: GPUI pinning via gpui-kit's `gpui-pre`, and staying current

- **Status:** Proposed
- **Date:** 2026-11-08

## Context
GPUI is pre-1.0 and lives in the Zed monorepo, with more than 100 commits to `crates/gpui` per quarter. It has had large API shifts:
- Jan 2025: `Model`/`View` became `Entity`, and `Context` was split from `Window`.
- Feb 2026: `gpui_platform` was split out of `gpui`.
- Feb 2026: Linux moved from Blade to wgpu.

There are three ways to depend on it:

| Option | State |
|---|---|
| crates.io `gpui` | Stuck at 0.2.2 (Oct 2025) |
| Weekly `gpui-pre` snapshots on crates.io | Published by Longbridge, Apache-2.0 |
| A Zed git rev | Used by Ghostex, tty7 and Wake, often via `[patch]` onto a fork |

We depend heavily on **gpui-kit** (formerly gpui-component, Longbridge, Apache-2.0). It pins `gpui = { package = "gpui-pre", version = "=0.3.x" }` exactly, and every gpui-related crate in the build must resolve to the same GPUI.

## Decision
- **Pinning:**
  - Depend on `gpui-kit` / `gpui-component` at an exact version.
  - Depend on the **same exact `gpui-pre`** version that gpui-kit pins.
  - Both are declared once, in `[workspace.dependencies]`.
- **Isolation:**
  - Only `loft-ui` and `loft-app` may depend on GPUI or gpui-kit.
  - Wrap the gpui-kit components we use in thin `loft-ui` widgets, so upstream API changes are absorbed in one place.
- **Upgrade cadence:** every 2–4 weeks, in a dedicated PR titled "bump gpui-kit X → Y". The PR includes:
  - a build;
  - a UI smoke test, using GPUI `TestAppContext` view tests;
  - the streaming benchmark scene;
  - a manual screenshot pass.
- **Escape hatch:** if upstream breaks us or we need a fix, use `[patch.crates-io]` to point at our own fork of gpui-kit, or `gpui-pre` swapped for a pinned Zed git rev. Upstream the fix, then drop the patch.

## Consequences
**Benefits**
- Reproducible builds with crates.io-only dependencies, and no Zed monorepo clone in CI.
- Always compatible with gpui-kit.
- We stay current incrementally instead of through rare, painful jumps.

**Costs and risks**
- We rely on the `gpui-pre` maintainer, so there is a bus-factor risk.
- We lag Zed main by roughly 1–2 weeks.
- Weekly gpui-kit 0.x breakage will be absorbed in bump PRs.

**Neutral**
- `gpui-fast`, the Longbridge performance fork, can be evaluated later as an opt-in feature.

## Alternatives considered
- **Pin a Zed git rev directly.** The freshest option, but it is incompatible with gpui-kit unless we also fork gpui-kit.
- **Vendor GPUI as a submodule fork, as Ghostex does.** Maximum control, but maximum maintenance.
- **Build all components ourselves on raw GPUI.** Too slow for v1, and Zed's own `ui` crate is GPL.

## References
- https://crates.io/crates/gpui-pre
- https://github.com/longbridge/gpui-kit
- https://github.com/zed-industries/zed/tree/main/crates/gpui
