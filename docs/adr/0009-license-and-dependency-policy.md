# ADR-0009: Apache-2.0 and a no-copyleft dependency policy

- **Status:** Accepted
- **Date:** 2026-11-08

## Context
The product owner wants to start public and keep the option to take the project private or commercial later. The ecosystem we draw from has mixed licenses:

| License | Projects |
|---|---|
| Apache-2.0 | GPUI, gpui-kit, `alacritty_terminal`, ACP crates, Codex |
| MIT | Ghostex, Claude Agent SDK |
| **GPL-3.0** | Zed's `editor`, `terminal`, `markdown`, `ui` and `theme` crates |
| **AGPL-3.0** | Vibex |

(This is not legal advice. Have counsel review before any commercial launch.)

## Decision
**License**
- Agentloft is licensed under **Apache-2.0**. It is permissive, includes an explicit patent grant, and matches GPUI and gpui-kit.

**Inbound contributions**
- Contributions come in under Apache-2.0 (inbound = outbound), with a DCO sign-off (`Signed-off-by`).
- Because the license is permissive, contributions can be included in a future proprietary version without relicensing. No CLA is needed.

**Dependency policy, enforced by `cargo-deny` in CI**
- **Allowed:** MIT, Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0, MPL-2.0 (file-level copyleft, unmodified use only), CC0.
- **Denied:** GPL, LGPL, AGPL, SSPL, BUSL.

**Copying code**
- Never copy from Zed's GPL crates or from Vibex.
- Code adapted from MIT or Apache sources (Ghostex, `gpui_tokio`, the Agent SDK) keeps its notices and is listed in `THIRD_PARTY_NOTICES.md`.

**Trademark**
- Apache-2.0 grants no trademark rights. Register "Agentloft" separately once it has been checked.

## Consequences
**Benefits**
- The project can go private or commercial at any time, using contributor code too.
- Broad compatibility with the ecosystem.

**Costs**
- Anyone may fork and commercialise the public versions. Versions already released stay Apache-2.0 forever.
- We cannot reuse Zed's excellent GPL editor, terminal and markdown crates.

## Alternatives considered
- **MIT.** Similar, but has no patent grant.
- **FSL-1.1-ALv2** (source-available, with a 2-year non-compete). It would protect a commercial product, but it is not open source and puts off contributors.
- **GPL-3.0.** It would unlock Zed's crates, but would block a closed-source future for any code derived from them.

## References
- https://www.apache.org/licenses/LICENSE-2.0
- https://embarkstudios.github.io/cargo-deny/
