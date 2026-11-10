# loft-terminal

A GPUI terminal widget backed by Ghostty's VT engine, libghostty-vt. It turns
PTY bytes into pixels and keystrokes into bytes. It never spawns processes:
the host (in Agentloft, the daemon) owns the PTY.

```rust
let terminal = cx.new(|cx| TerminalView::new(window, cx));
cx.subscribe(&terminal, |_, _, event: &TerminalEvent, _| match event {
    TerminalEvent::Input(bytes) => { /* write to the PTY */ }
    TerminalEvent::Resize { cols, rows, pixel_width, pixel_height } => { /* TIOCSWINSZ */ }
    TerminalEvent::TitleChanged(title) => { /* tab title */ }
    TerminalEvent::Bell => {}
})
.detach();
terminal.update(cx, |t, cx| t.feed(&pty_output, cx));
```

## Building

The `ghostty-vt-sys` crate builds libghostty-vt from Ghostty's source tarball
(downloaded once and checked against a pinned sha256), so the first build
needs:

- **Zig 0.17.x** on `PATH`, or its path in `$ZIG`.
- Network access, or `GHOSTTY_SOURCE_DIR` and `GHOSTTY_ZIG_SYSTEM_DIR` for
  offline builds.
- On macOS, the Xcode command line tools.

## Layout

| Module | Role |
|---|---|
| `backend/mod.rs` | `TerminalBackend`: the engine boundary (grid with damage, modes, key/mouse/paste/focus encoders, selection). Swappable, e.g. for `alacritty_terminal`. |
| `backend/ghostty.rs` | The libghostty-vt implementation. |
| `view.rs` | `TerminalView`: feed coalescing, events, keyboard, mouse, clipboard, IME. |
| `element.rs` | The GPUI element: cell metrics, per-row shaped cache, painting. |
| `input.rs` | GPUI keystroke → key event routing. |
| `theme.rs` | Colors from the gpui-kit theme and the ANSI palettes. |

## Demo

```sh
cargo run -p loft-terminal --example terminal_demo
```

It opens a window running your login shell through `portable-pty`. Set
`LOFT_TERMINAL_DEMO_THEME=dark|light` to pick a theme, or
`LOFT_TERMINAL_DEMO_SCRIPT=<file>` to replay a script of UI actions (see the
example's docs).
