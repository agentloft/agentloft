# Notes for coding agents

Read [CONTRIBUTING.md](CONTRIBUTING.md) first: commands, build speed, architecture rules. This file covers what an agent working in this repo needs on top of that. Whatever agent you are (Claude Code, Codex, OpenCode, …): when you learn something here that the next agent would need, add it to this file, not to your own private memory.

## Before you say a change is done

- `cargo fmt`
- `cargo clippy --workspace --all-targets --features loft-terminal/test-support -- -D warnings`
- `cargo test --workspace --features loft-terminal/test-support`
- Browser-only code (`cfg(target_arch = "wasm32")`) is invisible to native clippy: `RUSTC_BOOTSTRAP=wasm_thread cargo clippy -p loft-web --target wasm32-unknown-unknown -- -D warnings`.
- After touching dependencies or their features: `cargo hakari generate`.

Run the full suite once, in the background if your tool allows it, saving the output to a file you can grep. Don't run it twice.

## Dev and stable never share data

- The installed app (`cargo xtask install`, green icon) uses `~/.agentloft`. Dev bundles (`cargo xtask bundle`, blue icon) and debug builds use `~/.agentloft-dev`. An explicit `AGENTLOFT_HOME` overrides either.
- Test instances run on their **own fresh home** (e.g. under `/private/tmp`; `~/Desktop` is TCC-protected), seeded with made-up data. **Never copy the real `agentloft.db` into a scratch home.** A copy holds the user's threads and the pids of their live agents. A daemon's startup reaper (`crates/loftd/src/reaper.rs`) ends recorded agent processes it believes orphaned. It now skips any whose parent is a live `agentloftd`, but treat that as a safety net, not permission.
- Seeding: the `fake` harness (echoes the prompt as Markdown; `approve` opens an approval, `slow` streams slowly, `bgagents` leaves background subagents running, `fail` fails the turn, `limit:<seconds>` fails it as a used-up plan that resets that many seconds from now, `ui:all` / `ui:<sample>` show agent views) costs no tokens. Create threads over the daemon socket (`<home>/run/agentloftd.sock`; each frame is a big-endian `u32` length then JSON: first `{"type":"hello","protocol_version":1,"client_kind":"local-ui","client_build":"probe"}`, then `{"type":"request","id":1,"body":{"op":"create_thread",…}}`; a subscription's topic is `{"kind":"thread","id":…}`), or insert rows into the scratch `agentloft.db` while its daemon is stopped (backdate `updated_at_ms` by a day for the sidebar's "Older" group; `"status":"archived"` for "Archived").
- Fake CLIs (`gh`, …) for a scratch daemon: it reads its PATH from `$SHELL -l -i -c 'env -0'`, so launch it with `SHELL=<a script that prints env -0 with a stand-in bin dir first>`. In `loft-core` tests, `CoreOptions.env` sets PATH directly.
- A rebuilt app asks the running daemon to make way once, at launch (`Shutdown { reason: Upgrade }`, `crates/loft-client/src/native.rs`). The daemon refuses while a turn is running and does not count background work a thread is waiting on. So relaunching can leave an old daemon in place, or end other threads' background agents and shells. Check before swapping the user's app.

## Seeing a UI change without input access

- Launch: `AGENTLOFT_HOME=<scratch> target/debug/agentloft` (build with `cargo build -p loft-app -p loftd`). It opens the most recently active thread.
- `AGENTLOFT_DEV_TAB=<hook>` drives the first window: panels (`changes`, `terminal`, `browser`, `file:<path>`, `settings:<section>`, …), clicks (`click:x,y,after_ms;x,y,after_ms`, window points; `after_ms` counts from launch), `hover:x,y`, `type:<text>`, `action:<name>` (runs a project action), `palette:<query>`, `do:<steps>` (keys, typing, clicks, right-clicks and drags in order: `dev_script` in `crates/loft-ui/src/workspace/compose.rs`), `all:<hook>++<hook>` (several hooks in one launch), and more. The full list is `apply_dev_tab` in `crates/loft-ui/src/workspace.rs`. Add a hook there rather than posting synthetic OS input.
- Screenshot with `screencapture -x -o -l <window id>`, never a screen region (`-R`): that captures whatever of the maintainer's is in front. A menu is a small window of its own with its own id. Pick the test pid's largest on-screen layer-0 window (`CGWindowListCopyWindowInfo`); the app also owns small hidden windows. Delete screenshots afterwards.
- Documents (pptx/PDF/Office) without the app: `cargo build -p loft-ui --example doc_preview --features loft-ui/shots`, then `target/debug/examples/doc_preview <file> --shot out.jpg` (renders its own frame; no screen permission needed).
- Web client: `cargo xtask web [--release]` → `target/web`, served by the daemon (`[web] enabled = true` and a non-default `port` in the scratch home's `config.toml`; `agentloftd web --home <dir>` prints the pairing link). Drive it with headless Chrome over CDP using a fresh `--user-data-dir` per build, `Network.setCacheDisabled`, and `Page.bringToFront` (background tabs draw no frames).
- A window that isn't changing paints nothing, so also test after 30+ s idle.

## Gotchas

- New Lucide icons go in `AppIcons` in `crates/loft-ui/src/assets.rs`; an unlisted icon renders as a blank box.
- gpui-kit `Button` / `Switch` / dropdown buttons default to the arrow cursor; chain `.cursor_pointer()`. Everything clickable shows a hand.
- A hand-drawn control (a `div` with `on_click`) whose click calls `cx.stop_propagation()` also needs `.claims_press()` (`crates/loft-ui/src/press.rs`). The window's text selection starts on any press, even in blank space. Stopping propagation keeps the mouse-up from ending it, so it keeps following the pointer with no button held. gpui-kit's `Button` already claims its press.
- `AGENTLOFT_DEV_TAB=settings:<section>@end` opens that settings page scrolled to its end (what is below the fold). `AGENTLOFT_DEV_CLICK=x,y,after_ms` then clicks in the settings window.
- `AGENTLOFT_DEV_TAB=click:…` also takes `move x,y,after_ms` steps (the pointer moves with no button held), e.g. to check that nothing is left selecting after a click.
- A `drag_over` style can't recolour text (text is coloured at layout); show a drop hint with an svg and `group_drag_over`.
- A subagent's isolated worktree starts from `origin/main`, not the local `HEAD`: unpushed commits (and `press.rs`-style helpers they add) are missing there. Merge its files back with `git merge-file <ours> <base> <theirs>` per file, base being the worktree's `HEAD`.
- Looping animations go through `spinner::looping` / `Spinner`, not `with_animation(..).repeat()`, or the window redraws every display refresh.
- A new HTTPS call through `ureq` 3 must name its TLS provider in code (native-tls on macOS/Windows, rustls elsewhere) or it panics at the first request.
- Before building on a guess about AppKit, read it: `lldb -b -o 'target create <binary linking AppKit>' -o "disassemble -n '<method>'"`.

## How the maintainer likes work done

- Small UI fixes: make the obvious change, get it in front of them within minutes, and say early what is slowing things down.
- Use the platform's own mechanism. If a second workaround is piling up in the same area, find the root cause instead.
- Finish what can be done before reporting. A final "not done" list should only hold things that really can't be done, each with the reason.
- Report faithfully: what changed, how it was checked, what wasn't.
