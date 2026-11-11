# ADR-0018: Views agents draw in the chat

- **Status:** Proposed. Built on main on 2026-11-10 and reworked the same night at the product owner's request ("I want free-form… let agent be creative… remove components"); the choices marked "to review" are theirs to revisit.
- **Date:** 2026-11-11

## Context

- An agent's answer in Agentloft is text. Some answers are better seen or tried: test results, a size over time, three options to pick from, a calculator, a diagram that moves.
- Two products the product owner pointed at do this in different ways:
  - **ChatGPT's Intelligent UI** (October 2026): the model composes an answer from a fixed library of native components, with an escape hatch for a self-contained HTML app.
  - **T3 Code's MCP Apps**: an MCP server ships an HTML page and the app shows it in a frame inside the thread.
- The first version here (commit `1e8c049`) was the fixed-library kind: 27 blocks and a small expression language, drawn natively. The product owner's verdict was that a fixed set limits what an agent can make. This record replaces that design.
- Agentloft draws everything with GPUI, on the desktop and (as wasm on a canvas) in a browser. Neither has a place to put someone else's HTML that scrolls, clips and stacks like the rest of the timeline.
- The daemon already runs a headless Chrome and streams its pages to viewers ([ADR-0017](0017-streamed-browser.md)).
- Every agent Agentloft runs already gets the Agentloft MCP server ([ADR-0013](0013-agentloft-mcp-cross-thread-context.md)).

## Decision

**An agent calls `show_ui` with HTML of its own, and the thread shows it as a live page.** The page runs in the daemon's browser, where it can reach almost nothing, and is streamed into the timeline.

### 1. The tool

- `show_ui { title?, html }` on the Agentloft MCP server: Claude, Codex and ACP agents all have it with nothing per agent.
- `html` is a fragment or a whole document, with its own CSS and JavaScript. Anything a page can draw, a view can show.
- The tool's description is all an agent has to go on: the size it gets, the theme's variables, what loads and what doesn't, and `window.agentloft`. A test keeps it in step with what the page really gets.
- **The agent hears back what it can't see.** The daemon loads the page once (with nobody looking, if need be) and the result lists script errors, loads that were refused, and calls that were ignored, so the agent can fix the page and call again.

### 2. Where a view runs

A view runs in two kinds of place, written from the same HTML with the same head and the same bridge.

**In the desktop app (macOS): the real page.** The app hosts it in a web view of its own (WKWebView), so a click, a hover, the pointer, selection and typing are the web view's, with nothing in between.

- **Live under the pointer, a picture otherwise.** A native view draws above everything the app draws: menus, popups and tooltips would be under it, and it can't follow a scroll as smoothly as the rest. So the row shows the page itself only while the pointer is on it (or the keyboard is in one of its fields). The rest of the time it shows a picture the app took of that same page, which waits out of sight, still running. Going from one to the other shows nothing: the picture is taken as the pointer leaves, and the page comes in over the same pixels. A press on the picture brings the page in under it and is handed to it.
- **The thread scrolls through it.** The wheel over a shown page turns the thread, as over any row, and the page gives way to its picture while that lasts. The page says where it has something of its own for the wheel to turn (a list inside it, or itself when it is taller than a view gets): there the wheel is the page's, sideways always, up and down once the page was clicked into, so that scrolling past a view never catches.
- **The pointer is the page's.** The web view sets it itself; the app, which sets its own pointer again whenever it redraws, is told to want the same one there (as with the streamed page).
- **The keyboard is the page's once it is clicked into.** The app's own focus moves to the view with it, so Cmd+A, Cmd+C, Cmd+V and the rest go to the page through the Edit menu and not to the composer. A page with the keyboard in one of its fields stays live wherever the pointer goes; one that is out of sight keeps the keyboard, so what is selected in it can still be copied.
- **Cut where the list cuts it**, with the card's round corners, and kept out of the band at the bottom of the list where the app draws its fade and its jump button: there the picture shows, kept up with the page.
- **The same walls.** Its own address and storage (nothing of the user's browser), no navigation, and nothing loads but scripts, styles and fonts from the same hosts (WebKit content rules, in place before the page is). The bridge runs in a script world of its own, the only one that can post to the app, and still asks the browser whether the user really clicked.
- **A process each, while in sight.** A view out of sight for half a minute gives up its page; its picture stays, and the page comes back with what it remembered when it is drawn again.
- **The agent's check is unchanged**: the daemon still loads the page once in its own browser to say what went wrong in it.

**Everywhere else (a browser client, a system without a native web view, `AGENTLOFT_VIEWS=stream`): in a tab of the daemon's browser**, like the web client's browser tab: pictures out, input in. The rest of this section is about that.

- **Pictures of a real page.** So a view scrolls, clips and sits under menus like any other row, from one implementation.
- **A place of its own.** Each view lives at an address made up for it (`https://v<item>.view.agentloft.invalid/`), in a browser context apart from the user's tabs: its own storage and a secure context, none of their cookies.
- **Nearly no network.** Every request a view makes stops at the daemon first. It answers for the view's own document; lets `GET`s through to a short list of script, style and font hosts (jsDelivr, unpkg, cdnjs, esm.sh, Tailwind's CDN, d3js.org, Plotly's CDN, Google Fonts); and refuses the rest, navigations included. A link opens for the user instead; a page that leaves is brought back.
- **As wide as the chat column, as tall as its content** (24 to 1400 px; past that it scrolls inside, once clicked into). A view scrolls with the thread otherwise.
- **Sharp, and never a step behind.** Every picture of a view is a screenshot at the viewer's real scale, also while it moves. The browser's own stream of pictures only serves as the sign that something changed, and it is not a reliable one (Chrome drops what it can't send, the last change included). So a change, a click or a key is followed by looks at the page until one shows the same picture again.
- **The viewer's pointer is the page's.** The page says which cursor it wants where the mouse is (an I-beam over text that can be selected, a hand on a link or a button, whatever its CSS asks for), and the viewer's own pointer follows.
- **One thing at a time per page.** A screenshot is not a quiet thing in Chrome: while it lasts the page is drawn scaled to the viewer's pixel density, and when it ends the page is put back to the size it had when it began. So giving a page input, giving it a size and taking its picture never overlap.
- **It needs a browser.** Chrome, Chromium, Edge or Brave on the daemon's machine. Without one the view says so, and the agent is told the page was shown but not checked.

### 3. What a page gets

- **A start that matches the app.** Before the agent's HTML come a doctype, the theme as CSS variables (`--bg`, `--fg`, `--muted`, `--border`, `--surface`, `--accent`, `--accent-fg`, `--success`, `--warning`, `--danger`, `--font`, `--mono`), and base styles for plain elements, all in `:where()` so any rule of the page's wins. The card a view sits in is the page's background. The theme follows the viewer, live.
- **`window.agentloft`:**

| Call | What happens |
|---|---|
| `send(text)` | `text` goes to the agent as the user's message: sent now, or queued behind a running turn exactly as a typed message is |
| `setState(obj)` / `state` | JSON the view wants back next time. Kept by the daemon per view, so it survives reloads, restarts and other windows |
| `copy(text)` | To the clipboard of whoever clicked |
| `open(target)` | An `http(s)` link opens; anything else is a file of the thread's folder, shown in the side panel |
| `theme` | The colours and fonts as values; `agentloft:theme` fires on `window` when they change |

- **Only a real click speaks for the user.** `send`, `copy` and `open` work while the browser itself says the user just clicked or pressed a key. That is checked by a bridge script in a world of its own, which the page's script can neither see nor change, and the bridge alone can reach Agentloft. A page that calls `send` on load is ignored, and the agent is told.
- **Who clicked is who is answered.** With several viewers of one view, a `send`, `copy` or `open` goes to the connection whose input reached the page last.

### 4. As it is written

- Claude writes a tool call's input a piece at a time. For `show_ui` the item starts with the call, the HTML so far is passed on about every 240 bytes, and the daemon writes each new piece into the open page (`document.write`). The page appears as it arrives, the way a slow page loads: structure, then style, then script.
- What is half-written stays in memory. The log records the item three times: when it starts, when its input is whole, when it finishes.
- Codex and ACP agents report a call whole, so their views appear when the call is complete.

### 5. Where a view lives in the thread

- **In the `show_ui` tool call's own item.** The call's arguments are already an item of the thread, stored and sent to every client. A call that went through is drawn as a view; one that was turned down stays an ordinary failed tool line.
- **A harness that never reports the call** (it ran inside a code-execution tool) gets an item made for it: the bridge tells the daemon what was shown, and the daemon adds the item if the harness hasn't.
- A view is part of the answer: it stays in sight when the turn's work folds under "Worked for …".
- Pages are opened as their rows are drawn and let go when they haven't been on screen for a while; the daemon closes pages nobody watches after two minutes and keeps at most 20. A view that comes back into sight shows its last picture at once and reopens behind it, with what it remembered.

## Why not something else

- **A fixed library of native components** (the first version). Fast, exact and safe, but an agent can only say what the library has words for. Dropped at the product owner's request.
- **Only pictures on the desktop too** (how it was until 2026-11-11). One implementation, and correct under every menu; but every change cost a screenshot (50 to 100 ms), and a class of faults came with it: clicks that landed at half their coordinates while a screenshot was taken, pictures a step behind, no pointer or selection of the page's own. The product owner's verdict on learning it was pictures: "that's what was causing the issue.. can you add real embed??"
- **The real page always on top** (a web view that is simply there). Menus, popups, tooltips and the scroll-to-bottom button would be under it wherever they overlap, and it lags a scroll by a frame. Showing it only under the pointer keeps what is good about both.
- **An `<iframe>` over the canvas in the web client.** The same idea for browsers; not built. **To review.**
- **Letting pages reach the network.** Agents like to pull charts' libraries from a CDN, so a few script hosts are let through. More than that would make a view a way around the user's approvals: a page could send out whatever the agent put in it.
- **MCP Apps** (pages that MCP servers ship with their tools, as T3 Code shows them). Looked into and set aside at the product owner's request on 2026-11-11, before any of it was built: `show_ui` is the way an agent draws here. Should it come back, the same view pages can show them: Claude Code hands a host what it needs with `CLAUDE_CODE_MCP_APPS_HOST=1` and the `mcp_status`, `mcp_read_resource` and `mcp_call` control requests; Codex marks a call with `mcpAppUi` and has `mcpServer/resource/read`; ACP has nothing.
- **A new item kind.** The tool call's item already holds everything; a second kind would be a protocol change that buys nothing now that the daemon can add the item itself.

## Consequences

- An agent can answer with anything a web page can be, and ask a question whose answer is a click.
- In the desktop app a view is the page: native speed, the page's own pointer, selection and typing. A view that moves by itself is shown at about a dozen pictures a second while the pointer isn't on it. In a browser client it is pictures of the page: no native text selection or screen reader inside it, and about 20 pictures a second.
- Each view in sight is a web view process in the app (or a tab in the daemon's browser, twenty at once at most, for browser clients).
- The description costs every session about 500 tokens of tool definitions.
- `read_thread` shows a view as its HTML, not as a picture.
- A page is only as good as the agent's HTML. The check at the end of `show_ui` catches what throws or doesn't load, not what merely looks wrong.

## What's left

- Images a view may load: none from the web today, and none from the thread's folder.
- The real page on other systems than macOS, and in the web client (an `<iframe>`).
- A view shown in a second window of the app keeps its page in the first.
- Letting the agent read what a view remembered, rather than hearing it in a `send`.

## References

- `crates/loft-view` (what a view is, the page's head and bridge, samples), `crates/loft-core/src/pages/views.rs` (the page in the daemon's browser), `crates/loft-ui/src/view.rs` and `panel/page.rs` (the row and its pictures), `crates/loftd/src/mcp.rs` (`show_ui`).
- Try it without an agent: a thread with the `fake` agent and the prompt `ui:all` (or `ui:tests`, `ui:pricing`, `ui:approach`, `ui:orbit`; `ui:stream:pricing` writes one out slowly; `ui:<p>…` shows a page of your own).
- Real browser test: `cargo test -p loft-core pages::views::tests::live -- --ignored`.
