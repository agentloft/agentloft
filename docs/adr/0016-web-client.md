# ADR-0016: Agentloft in a browser: the same UI as wasm, over a secured listener in the daemon

- **Status:** Accepted. Merged to main on 2026-11-10; the decisions marked "to review" are still the product owner's to revisit.
- **Date:** 2026-11-10

## Context

Agentloft's core is a daemon ([ADR-0001](0001-daemon-from-m0.md)) and the GPUI app is one of its clients. We want Agentloft usable from a web browser:

- on the same machine first;
- later from another machine or a phone ([ADR-0014](0014-remote-access-tailscale.md)).

Two constraints shape everything:

1. **No second UI to maintain.** A separate React (or other) front end would have to re-implement the timeline, composer, approvals, diff view and settings, and keep up with the native app forever.
2. **It must be secure.** A browser session can ask agents to run commands on the user's machine. A listening port is a new attack surface, and browsers add their own attacks (other sites, DNS rebinding).

## Decision

### 1. One UI: `loft-ui` compiled to WebAssembly

- The browser runs the **same `loft-ui` crate**, compiled for `wasm32-unknown-unknown` and drawn by GPUI's web platform (`gpui-pre-web` 0.3.11, the snapshot of Zed's `gpui_web` that matches our pinned `gpui-kit` 0.9.0, [ADR-0005](0005-gpui-pinning-and-staying-current.md)). It renders to a canvas with WebGPU, falling back to WebGL2.
- `crates/loft-web` is the wasm entry point. `cargo xtask web [--release]` builds it, generates the JS glue and writes the bundle to `target/web`.
- What differs between the desktop and the browser is behind one module, **`loft-ui/src/host.rs`** ("the machine the daemon runs on, as the UI reaches it"):

| The UI needs | Desktop | Browser |
|---|---|---|
| A connection | Unix socket; starts the daemon | WebSocket to the page's own origin |
| `config.toml`, themes, sidebar state | The files | An in-memory mirror from `UiFiles`, re-asked every 3 s; writes go out as `ChangeSetting`, `SaveTheme`, `DeleteTheme`, `PutUiState` |
| Images (replies, attachments, icons) | GPUI loads the path | `ReadPath` / `ProjectIcon`, cached |
| Window layout | `ui-session.json` | The browser's `localStorage` (it is that browser's own) |
| The ⌘ / Ctrl choice | Compile target | The device the browser runs on |

- Views call `host::…` and stay the same. They ask `host::IS_WEB` only to leave out what a browser cannot do.

**The terminal is the same widget too**

- `loft-terminal` and its emulator run in the browser unchanged. libghostty-vt builds for `wasm32-freestanding` as a static archive (Zig, no libc, SIMD off), and links into the wasm like any other object. It costs about 0.25 MB of the gzipped download.
- The published `ghostty-vt-sys` knows no wasm target and checks 64-bit layouts, so the repository carries a copy with those two things added (`vendor/ghostty-vt-sys`, patched in through `[patch.crates-io]`). Its README lists what differs and how to regenerate the wasm32 bindings.
- The PTY is the daemon's, as on the desktop (ADR-0007): the browser sends keystrokes and gets output over the same requests.
- What a browser changes:
  - Paste comes from the browser's own paste (⌘V, Ctrl+Shift+V, its menu), because a page cannot read the clipboard by itself. It is bracketed like any paste.
  - Copy and Select All answer to both ⌘ and Ctrl+Shift.
  - Keys the browser keeps never arrive: ⌘W / Ctrl+W, ⌘T, ⌘N, ⌘Q.
  - Fonts are the shipped JetBrains Mono, so prompt glyphs from icon fonts are missing.

**Native-only pieces in the browser build**

| Piece | In the browser |
|---|---|
| Embedded browser (WKWebView) | Replaced by a page of the daemon's own browser, streamed ([ADR-0017](0017-streamed-browser.md)). |
| Folder browser, file dialogs, Finder, "Open in editor" | Left out. |
| Menus and Settings in their own window | Drawn inside the one window. |
| Dock badge, notifications | None. |
| Syntax highlighting | syntect instead of tree-sitter (its C grammars do not build for wasm32). |
| System fonts | Not reachable: the client ships Inter, JetBrains Mono and IBM Plex Sans (all OFL-1.1). CJK and emoji use the browser's fonts through GPUI's canvas fallback. |

**Build notes**

- `gpui-kit` always enables `gpui-pre-web`'s `multithreaded` feature, whose `wasm_thread` dependency uses one unstable feature gate. The build sets `RUSTC_BOOTSTRAP=wasm_thread`, which allows it for that one crate on our pinned stable compiler.
- We run **single-threaded** (`allow_multi_threading = false`). Threads would need `SharedArrayBuffer`, which needs cross-origin isolation headers and a nightly `build-std`. Not worth it now.
- The JS glue is generated in-process by `wasm-bindgen-cli-support`, pinned to the lockfile's `wasm-bindgen`. No external tool to install or keep in step.

### 2. Transport: an HTTP + WebSocket listener in `agentloftd`

- `agentloftd` gets a listener (hyper 1, tokio-tungstenite, rustls) that serves the bundle and carries the **same `ClientMsg` / `ServerMsg` JSON** as the Unix socket: one message per WebSocket text message.
- The connection loop (`loftd/src/conn.rs`) is shared by both transports. It knows who is on the other end: `Peer::Local` (the Unix socket) or `Peer::Remote` (a browser session).
- **Off by default.** `[web]` in `config.toml`:

```toml
[web]
enabled = true
bind = "127.0.0.1"          # default
port = 4780                 # default; 0 picks a free port
tls_cert = "~/certs/desk.pem"
tls_key  = "~/certs/desk.key"
allowed_hosts = ["desk.example.ts.net"]
allow_insecure_remote = false
session_days = 30
allow_terminal = true       # default: true while only this machine can reach the listener
allow_browser = true        # the same rule, for the daemon's browser (ADR-0017)
```

- The listener follows the config: a change restarts it; a section that breaks the rules leaves it off, with the reason in its status.

### 3. Security model

**Listening**

- Binds `127.0.0.1` unless told otherwise.
- A non-loopback address is **refused** unless TLS is configured (certificate and key) or `allow_insecure_remote` is set. The desktop app warns loudly about any non-loopback address, and more loudly about an unencrypted one.

**Authentication**

- **Pairing token:** 256 random bits from the OS, in `<home>/web/token` (file 0600, folder 0700). Compared in constant time (SHA-256 of both sides, then `subtle`). Rotatable; rotating signs every browser out.
- **Pairing:** the desktop app shows a link, `http://127.0.0.1:4780/#token=…`. The token is in the URL **fragment**, which browsers never send to a server, so it stays out of request logs and `Referer`. The page reads it, removes it from the address bar, and POSTs it once to `/api/pair`.
- **Session:** the reply sets a cookie holding another 256-bit secret: `HttpOnly`, `SameSite=Strict`, `Path=/`, and over HTTPS `Secure` with the `__Host-` prefix. Page scripts never see it. Only its SHA-256 is kept, in memory and in `<home>/web/sessions.json` (0600), so that file signs nobody in.
- Sessions expire (`session_days`, default 30), can be revoked one at a time or all at once, and a revoked session's WebSockets are closed at once (close code 4401, which sends the page back to pairing).
- **Rate limiting:** five wrong tokens from one address (one IPv6 /64) in a minute lock it out for a minute, right token included; fifty from everywhere trip a global brake.
- Everything but the page shell needs a session: the app's files, `/api/session`, and the WebSocket upgrade.

**Browser attacks**

| Attack | Defence |
|---|---|
| DNS rebinding (an attacker's domain re-pointed at 127.0.0.1) | `Host` must be an IP literal, `localhost`, or a name in `allowed_hosts`. A rebound page still sends its own name. |
| Cross-site WebSocket hijacking; CSRF from another site or another local port | `Origin` must be the origin the request was addressed to, on the WebSocket upgrade and on every POST. `Sec-Fetch-Site` is honoured. A cookie with no `Origin` is refused. |
| Reading our responses from another origin | No CORS headers, ever; preflights get 405. |
| Clickjacking, content sniffing, script injection | `frame-ancestors 'none'` and `X-Frame-Options: DENY`; `nosniff`; CSP `default-src 'none'; script-src 'self' 'wasm-unsafe-eval'`. No inline script. Inline style *attributes* are allowed because GPUI sizes its canvas with them. |
| Flooding | 16 MiB WebSocket messages (`MAX_FRAME_LEN`), 32 KiB request heads, 4 KiB pairing bodies, 128 connections, 32 WebSockets, 10 s for a request head or TLS handshake, slow readers dropped. |
| Path traversal in the file server | Files are served from a list made by reading the bundle folder. A request path is compared with the list and never joined to a directory; symlinks are not listed. |

**Least privilege for a browser session** (`loft_core::RemotePolicy`, an allow-list)

- May: everything about threads and conversations, approvals, appearance settings, files and diffs inside a project or thread folder.
- May not:
  - stop the daemon, or manage web access (see the pairing link, rotate it, revoke sessions);
  - change what runs: agents' binary paths, launch arguments or environment (`harness.*.path|extra_args|env`), or the project folders (`projects.*`);
  - open a terminal, unless terminals are allowed. Unset, `allow_terminal` follows who can reach the listener: yes for a loopback address with no `allowed_hosts`, no once another address, a proxy or a tunnel is involved. Set, it is as set. A session learns which in its `Welcome`, and a pane without the right says where to turn it on;
  - add, create or clone a project anywhere but where projects are kept (`projects.new_dir`, and `projects.base_dir` if the user set one);
  - read a file by path outside project roots, thread folders (worktrees), and Agentloft's `attachments` and `icons`. Paths are canonicalized, `..` is refused, and a symlink that leaves those folders is refused. "Outside" and "missing" get the same answer.
  - use the agent-facing requests (`spawn_thread`, `list_threads`, …).
- Sees `config.toml` with agents' environment values replaced and `[web]` removed.
- Its requests are logged by name (`agentloft::web::audit`).

### 4. Threat model

**In scope**

- Other websites open in the user's browser, including ones that guess the port.
- Other servers on the same machine (dev servers, an agent's preview) that share the `localhost` cookie jar.
- Other users on the same machine (file modes; the Unix socket already checks the peer's uid).
- Machines on the network, when the listener is bound beyond loopback.
- Someone who gets a copy of `sessions.json`, or of a request log.

**Out of scope, stated plainly**

- **A browser session is as powerful as a shell.** It can ask an agent to run commands and approve them, and on a local-only listener it gets a terminal outright. `RemotePolicy` is hygiene: it stops a stolen session from making itself permanent (new binary paths, web settings) or wider (arbitrary files; a terminal, once the listener is reachable from elsewhere). It is not a sandbox.
- A process already running as the user. It can read the token file and the Unix socket.
- A pairing link the user gives away. Treat it like a password; "New link" replaces it.
- Cookies are not port-scoped: another server on `localhost` receives ours. It cannot use it from a browser (the `Origin` check), and a remote attacker cannot reach a loopback listener at all. Over HTTPS the `__Host-` cookie is tied to the host.

### 5. Designed for remote use later

Nothing here needs redesign to be reached from another machine or a phone:

- **Behind a tunnel or reverse proxy** (`tailscale serve`, as [ADR-0014](0014-remote-access-tailscale.md) prefers): keep `bind = 127.0.0.1` and add the public name to `allowed_hosts`. The proxy ends TLS; we see the browser's `https://` origin and set `Secure`, `__Host-` cookies.
- **Directly:** set `bind` and a certificate.
- **A native remote client** (the desktop app connecting to another machine) pairs for a bearer session (`POST /api/pair {"bearer": true}`) and sends it in `Authorization`. No `Origin` is needed, because nothing sends a bearer token automatically.
- ADR-0014's second factor (a per-device ed25519 key, approved on the host) can replace the bearer token without touching the transport, the policy or the UI.

## Consequences

**Benefits**

- One UI codebase. A view added to `loft-ui` appears in the browser too.
- The protocol is unchanged, so features work in the browser without new endpoints.
- The security-sensitive code is small, in one place (`loftd/src/web/`, `loft-core/src/remote.rs`) and covered by tests: 11 policy tests, 25 unit tests and 32 end-to-end tests against a real listener.

**Costs**

- The download is large: about the size of the app. See TRACKING for measured sizes.
- GPUI's web platform is young and single-window. We lean on a `gpui-pre-web` snapshot, and on one unstable feature gate.
- Canvas rendering means no browser text selection, no browser find, no screen-reader support, and no browser extensions seeing the page. GPUI's own selection and IME work.
- Syntax colours differ slightly between the desktop (tree-sitter) and the browser (syntect).
- `host.rs` is a second implementation of file access to keep right.

**Neutral**

- Phone-sized layouts are not done; the UI is the desktop layout.

## Decisions to review

1. **`RUSTC_BOOTSTRAP=wasm_thread`** for the wasm build (one crate, pinned compiler) versus a patched `gpui-kit`.
2. **Port 4780** as the default.
3. **A long-lived pairing token** (as asked for) rather than one-time codes. One-time codes would be safer against a leaked link.
4. **Rotating the link signs everyone out.**
5. **The browser allow-list**, especially: projects only where projects are kept; settings a browser may change.
6. **Shortcut defaults in the browser:** new thread Ctrl+Alt+N, new pane Ctrl+Alt+T, settings Ctrl+Alt+, (the browser keeps ⌘N, ⌘T and ⌘,).
7. **Shipping fonts** (Inter, JetBrains Mono, IBM Plex Sans; about 3 MB before compression).
8. **Terminals in the browser are on for a local-only listener** and off, until asked for, once other machines can reach it. The alternative is off everywhere until asked for.
9. **A patched copy of `ghostty-vt-sys` in `vendor/`** (wasm target, wasm32 bindings) until the published crate has them.

## Alternatives considered

- **A second UI in TypeScript.** The fastest path to a small, native-feeling web app, and exactly what was ruled out: two UIs to keep in step.
- **Streaming the native window** (VNC-style). One UI, but poor on a phone, laggy, and it needs the desktop app running.
- **Hand-rolled HTTP and WebSocket parsing.** Fewer dependencies, more risk in the one place that must not have bugs. hyper and tungstenite are small, permissive and well tested.
- **The token in the URL query.** It would land in history, logs and `Referer`.
- **The session in `localStorage`.** Readable by any script on the page; an `HttpOnly` cookie is not.
- **Multi-threaded wasm.** Needs nightly `build-std` and cross-origin isolation; no need yet.

## References

- [ADR-0014](0014-remote-access-tailscale.md): remote access over Tailscale or SSH
- `crates/loftd/src/web/`, `crates/loft-core/src/remote.rs`, `crates/loft-ui/src/host.rs`, `crates/loft-web/`
- `crates/loftd/tests/web.rs`
- Zed's `gpui_web` (Apache-2.0), via `gpui-pre-web`
