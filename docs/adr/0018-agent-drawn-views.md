# ADR-0018: Views agents draw in the chat

- **Status:** Proposed. Built on main on 2026-11-10 at the product owner's request ("something similar that agents can use"); the choices marked "to review" are theirs to revisit.
- **Date:** 2026-11-10

## Context

- An agent's answer in Agentloft is text. Some answers are better seen: test results, a size over time, three options to pick from, a number that depends on two others.
- Two products the product owner pointed at do this in different ways:
  - **ChatGPT's Intelligent UI** (October 2026): the model composes an answer from a fixed library of native components (charts, forms, sliders, buttons). The app draws them; state changes stay in the app, and a button can start a new turn.
  - **T3 Code's MCP Apps**: an MCP server ships an HTML page and the app shows it in a frame inside the thread.
- Agentloft draws everything with GPUI, on the desktop and (as wasm on a canvas) in a browser. It has no DOM to put someone else's HTML in without a web view floating over a scrolling timeline.
- Every agent Agentloft runs already gets the Agentloft MCP server ([ADR-0013](0013-agentloft-mcp-cross-thread-context.md)).

## Decision

**Agents call `show_ui` with a tree of blocks, and Agentloft draws it natively in the thread.**

### 1. The tool

- `show_ui { title?, state?, body: [block] }` on the Agentloft MCP server, so Claude, Codex and ACP agents all have it with nothing per agent.
- The bridge only **checks** the view and answers at once: "Shown in the chat", plus notes for properties it ignored, or what to fix. A view that doesn't pass is not shown, and the agent is told why in words it can act on (the block's path, the names it could have meant).
- The tool's description is the whole component reference, since it is all an agent has to go on. A test keeps its example valid and every block type named.

### 2. The blocks

Twenty-seven, in five groups (`loft-view`, `TYPES`):

| Group | Blocks |
|---|---|
| Layout | `row`, `column`, `grid`, `card`, `tabs`, `details`, `divider` |
| Content | `text`, `heading`, `markdown`, `badge`, `stat`, `progress`, `list`, `key_value`, `steps`, `callout`, `code` |
| Data | `table`, `chart` (bar, line, area, pie, donut) |
| Inputs | `slider`, `input`, `select`, `choice`, `checkbox`, `toggle` |
| Actions | `button` |

- Any block takes `when`: it shows while the expression holds.
- Rows of a table or chart are written out, or generated from a range (`{ for, from, to, step?, row }`), so a chart can follow a slider.
- Reading is lenient where the meaning is plain: a string where a block goes is a paragraph, common other names map to ours (`metric` is `stat`, `dropdown` is `select`), unknown properties are notes, not errors.

### 3. State and expressions

- A view has state: what the agent gave in `state`, plus one value per input (`bind`).
- Any text may hold `{{ expressions }}` over that state: arithmetic, comparisons, `a ? b : c`, lists, and about thirty functions, most of them for writing numbers the way people read them (`usd`, `percent`, `bytes`, `duration`).
- The language is a small parser and evaluator of our own. It has no loops, no assignment and nothing to read but the view's state; expressions and nesting are capped, as are blocks (400), depth (8), rows (500) and the spec's size (200 KB).
- A name an expression reads must exist, or the view is turned down. Without that check a misspelt name would show the user a blank where a number belongs.
- State lives in the window showing the view and starts over when the thread is loaded again. **To review:** keep it in the daemon, so every window and a relaunch see the same slider position.

### 4. What a view can do

One thing only: a button (or a list row) with an action.

| Action | What happens |
|---|---|
| `send` | The text goes to the agent as the user's message: sent now, or queued behind a running turn exactly as a typed message is. It shows in the thread as theirs |
| `set` | The view's own state changes |
| `copy` | Text goes to the clipboard |
| `open` | An `http(s)` link opens; anything else is a file of the thread's folder, shown in the side panel |

- Hovering a `send` button shows the words it will say, so a label can't hide what is sent in the user's name.
- Nothing else leaves a view. The agent cannot read the state; it learns what the user chose when a `send` says so.

### 5. Where it is drawn

- **From the tool call itself.** The call's arguments are already an item of the thread, stored and sent to every client. A `show_ui` call that went through is drawn as a view where it stands in the timeline; one that was turned down stays an ordinary failed tool line. No new item kind, no daemon state, nothing to migrate.
- A view is part of the answer: it stays in sight when the turn's work folds under "Worked for …".
- Charts use fixed series colours in a fixed order, stepped separately for light and dark, with a legend from two series up. A pie folds what is past six slices into "Other".

## Why not something else

- **HTML from the agent (or MCP Apps) in a frame.** Needs a web view over a virtualized, scrolling timeline on the desktop and an iframe over a canvas in the browser; looks like someone else's product; and runs script the agent wrote. MCP Apps from third-party servers may still be worth having one day, as their own thing.
- **A fenced block in the answer's Markdown.** Streams with the text and saves a tool round trip, but every agent would need the format in its prompt, and a block that doesn't parse can't be sent back to the agent to fix.
- **A new item kind written by the daemon.** Would also catch agents that call tools from inside a code-execution tool (OpenCode's code mode), at the price of a protocol change and a second copy of what the tool call already holds. **To review** if that case matters.
- **A general scripting language for state.** More than a calculator needs, and something to sandbox.

## Consequences

- An agent can answer with a chart, a table or a form, and ask a question whose answer is a click.
- The component set is ours to keep stable: agents learn it from the tool's description, and old threads hold old specs. Blocks may be added; a block's meaning shouldn't change.
- A view an agent drew through a code-execution tool (no `show_ui` call of its own in the thread) is not drawn.
- The description costs every session about 700 tokens of tool definitions.
- `read_thread` shows a view as its spec, not as a picture.

## What's left

- State kept in the daemon (see 3).
- A view that streams in as the agent writes it.
- Images, and icons on buttons.
- A way for a view to wait for its answer (a blocking form), rather than hearing it as the next message.

## References

- `crates/loft-view` (spec, expressions, samples), `crates/loft-ui/src/view.rs` (drawing), `crates/loftd/src/mcp.rs` (`show_ui`).
- Try it without an agent: a thread with the `fake` agent and the prompt `ui:all` (or `ui:tests`, `ui:pricing`, `ui:deploy`, `ui:approach`, `ui:release`, `ui:sizes`, or `ui:{…}` with a spec of your own).
