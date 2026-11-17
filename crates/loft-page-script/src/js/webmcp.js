// Tools a page offers agents through WebMCP (`navigator.modelContext`).
//
// The body of an async function, like agent.js: the host prepends
// `const ARGS = {...};` and appends `return JSON.stringify(await loftWebmcp(ARGS));`.
// It runs in the helpers' isolated world first and, when WebMCP is only
// there for the page's own scripts (a polyfill), in the page's world:
// everything here is local to the function, so nothing is left behind.

const WEBMCP_MAX_TOOLS = 100;
const WEBMCP_MAX_NAMES = 50;
const WEBMCP_MAX_SCHEMA = 32000;

// The page's model context, if it has one that lists tools.
function webmcpContext() {
  for (const owner of [document, navigator]) {
    let context = null;
    try { context = owner.modelContext; } catch (_) { /* not readable */ }
    if (context && typeof context.getTools === 'function') return context;
  }
  return null;
}

// The tools this window offers (a tool tied to another window doesn't count).
async function webmcpTools(context) {
  const listed = await context.getTools();
  let tools = [];
  if (Array.isArray(listed)) tools = listed;
  else if (listed && typeof listed[Symbol.iterator] === 'function') tools = [...listed];
  return tools.filter((tool) => tool && typeof tool === 'object' &&
    (tool.window == null || tool.window === window || tool.window === globalThis));
}

// A tool as an agent reads it.
function webmcpDescribe(tool) {
  const out = {
    name: typeof tool.name === 'string' ? tool.name : '',
    description: typeof tool.description === 'string' ? tool.description.slice(0, 1000) : '',
  };
  if (typeof tool.title === 'string') out.title = tool.title.slice(0, 200);
  let schema = tool.inputSchema;
  if (typeof schema === 'string') {
    try { schema = JSON.parse(schema); } catch (_) { schema = undefined; }
  }
  if (schema && typeof schema === 'object') {
    let json = null;
    try { json = JSON.stringify(schema); } catch (_) { /* not data */ }
    if (json && json.length <= WEBMCP_MAX_SCHEMA) out.inputSchema = JSON.parse(json);
  }
  const notes = tool.annotations;
  if (notes && typeof notes === 'object') {
    const first = (...values) => values.find((v) => v !== undefined);
    out.annotations = {
      readOnly: !!first(notes.readOnlyHint, notes.readOnly),
      consequential: !!first(notes.consequentialHint, notes.consequential),
    };
  }
  return out;
}

// Chrome's own `executeTool` may want the input as JSON text.
function webmcpWantsText(error) {
  const message = String((error && error.message) || error || '');
  return !!error && error.name === 'TypeError' &&
    (message.includes("is not of type 'object'") || message.includes('Failed to parse input arguments'));
}

async function webmcpRun(context, tool, input) {
  if (typeof context.executeTool === 'function') {
    try {
      return { ran: true, value: await context.executeTool(tool, input) };
    } catch (error) {
      if (!webmcpWantsText(error)) throw error;
      return { ran: true, value: await context.executeTool(tool, JSON.stringify(input)) };
    }
  }
  if (typeof context.invokeTool === 'function') {
    return { ran: true, value: await context.invokeTool(tool.name, input) };
  }
  if (typeof tool.execute === 'function') return { ran: true, value: await tool.execute(input) };
  return { ran: false };
}

async function loftWebmcp(args) {
  const context = webmcpContext();
  if (!context) return { ok: true, available: false, tools: [] };
  const tools = await webmcpTools(context);

  if (!args.call) {
    return {
      ok: true,
      available: true,
      tools: tools.slice(0, WEBMCP_MAX_TOOLS).map(webmcpDescribe),
      omitted: Math.max(0, tools.length - WEBMCP_MAX_TOOLS),
    };
  }

  const tool = tools.find((t) => t.name === args.call);
  if (!tool) {
    const names = tools.map((t) => (typeof t.name === 'string' ? t.name : '')).filter(Boolean);
    const offered = names.length
      ? 'it offers ' + names.slice(0, WEBMCP_MAX_NAMES).join(', ') +
        (names.length > WEBMCP_MAX_NAMES ? ', …' : '')
      : 'it offers none';
    return {
      ok: false,
      code: 'no_tool',
      error: 'the page has no WebMCP tool ' + JSON.stringify(args.call) + '; ' + offered,
    };
  }
  const input = args.input && typeof args.input === 'object' ? args.input : {};
  const run = await webmcpRun(context, tool, input);
  if (!run.ran) {
    return {
      ok: false,
      code: 'no_call',
      error: 'the page lists the WebMCP tool ' + JSON.stringify(args.call) +
        ' but offers no way to run it',
    };
  }
  let result = run.value;
  if (result === undefined) result = null;
  else if (typeof result === 'string') {
    try { result = JSON.parse(result); } catch (_) { /* plain text */ }
  }
  return { ok: true, result };
}
