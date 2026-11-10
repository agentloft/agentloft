# ADR-0017: A browser the daemon runs, streamed to the web client

- **Status:** Proposed. Built on `feat/web-client`; the decisions marked "to review" are the product owner's to confirm.
- **Date:** 2026-11-10

## Context

- The desktop app's browser panel is a native web view inside the app (WKWebView, `loft-browser`). The web client ([ADR-0016](0016-web-client.md)) has no such thing to embed: it is a page itself, and the pages a user wants to look at (a dev server on the daemon's machine, a site behind that machine's logins) are not reachable from wherever the browser tab runs.
- One answer to the same problem is a "server browser": a real Chromium on the machine that runs the agents, shown to web viewers as a stream of pictures and driven by sending their input back.

## Decision

**The daemon runs its own browser and streams its pages.** The web client's Browser tab is a view of one of those pages. The desktop app keeps its native panel for now.

### 1. The browser

- A Chromium-family browser that is already installed (Chrome, Chromium, Edge or Brave; `[browser] executable` picks one). Agentloft doesn't ship one.
- Headless, with a profile of its own under the Agentloft home (`browser/profile`, mode 0700), so its cookies and logins are separate from the user's everyday browser.
- Driven over the DevTools protocol on a **pipe** (`--remote-debugging-pipe`), not a port. Only the daemon holds the pipe; no other program on the machine, and no web page, can attach to it.
- Started with the first page, asked to close when the daemon stops, and stopped five minutes after its last page closes.
- Settings ▸ Browser (`[browser]` in `config.toml`) holds what both browser panels share, the search engine and the home page, and which browser the daemon runs. A browser session may change the first two, not the last.
- Downloads are refused. A window a page opens loads in the page that opened it. A script dialog is answered for the page (yes to notices and to leaving, no to questions) until dialogs are shown to the viewer.

### 2. Pages and pictures

- One page per thread (`PageOpen`), kept the size of the view that shows it.
- While someone watches (`Topic::Page`), Chrome's screencast sends JPEG pictures. The daemon keeps only the newest and announces it; each viewer is sent it when it has acknowledged the last one (`PageFrameAck`). **One picture is in flight per viewer**, so a slow link gets fewer pictures instead of a growing queue. Nobody watching, no pictures.
- Chrome is acknowledged no sooner than 70 ms after a picture, which caps the stream at about 14 a second.
- Screencast pictures are one device pixel per CSS pixel. When a page holds still for 250 ms, one screenshot at the viewer's real scale replaces them, so text is sharp on a high-density screen without paying for it while scrolling.
- Pictures travel as base64 inside the same JSON messages as everything else.

### 3. Input

- The viewer sends what it did (`PageInput`): mouse, wheel, keys as a DOM `key` and `code`, and text. The daemon replays it into the page as real input events (`Input.dispatch…`), in the order it was sent: one task per page, fed from the connection's ordered path, as terminal keystrokes are.
- A typed character goes as its key press, so pages that listen for keys work. A paste or an input method's text goes as text.
- Copy and cut read the page's selection and put it on the **viewer's** clipboard. The page's own clipboard is the daemon machine's and is left alone.
- On a Mac, the editing commands that the system's key bindings would have produced (⌘A, ⌘Z, ⌥←, …) are named explicitly, because a headless page gets none of them otherwise.

### 4. Who may use it

- Local clients: always.
- A browser session: `[web] allow_browser`, with the same rule as terminals. Unset, it is allowed while only this machine can reach the listener (a loopback address and no `allowed_hosts`) and refused once another address, a proxy or a tunnel is involved. Settings ▸ Web access has the switch. The session learns which in its `Welcome`.
- A page may be sent to `http`, `https` and `about:blank` addresses only: not to local files, not to the browser's own pages.

## Why not something else

- **Stream the desktop app's WKWebView.** It has no screencast and no way to inject trusted input; it would only work while the desktop app runs, and badly.
- **Proxy the page into an `<iframe>`.** Works only for sites that tolerate being framed and rewritten, puts another origin's script next to ours, and gives an agent nothing to drive.
- **Bundle a browser.** About 150 MB for something most machines already have.
- **A debugging port.** Any local program, including one run by another user of the machine, could attach to a browser that holds the user's logins.

## Consequences

- The web client's browser needs Chrome (or a sibling) on the daemon's machine. Without one the tab says so.
- The web client and the desktop app don't share browser tabs yet: one is the daemon's Chrome, the other the app's WebKit view.
- Agents still drive the desktop app's panel (`BrowserCall`). With only a web client connected they have no browser.
- A browser on the daemon's machine reaches what that machine reaches (its localhost, its network) and keeps logins. That is the point, and it is why a session reachable from elsewhere doesn't get it unless asked for.
- A picture is a picture: no text selection drawn by the viewer's system, no native menus, no screen readers inside the page.

## What's left

- Agents on the daemon's browser (the `BrowserCall` commands over the same DevTools connection), with one controller at a time: the agent or a person.
- The desktop app showing the same pages (so app, web and agents share tabs).
- Dialogs, file choosers and downloads shown to the viewer; several tabs per thread; the pointer's shape; an input method's text shown while it is being composed.
- Smaller, moving pictures on a slow link (half scale while scrolling), and binary frames instead of base64.

## Decisions to review

1. **A daemon-owned Chrome for the web client, the native view staying in the desktop app.** The alternative is one browser for both, with the desktop panel becoming a stream too.
2. **Using the installed browser** rather than shipping one.
3. **On for a local-only listener**, off until asked for once other machines can reach it (as for terminals).
4. **Its own profile**: nothing is shared with the user's everyday browser, so sites need signing in to once more there.
