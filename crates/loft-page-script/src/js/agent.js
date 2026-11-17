// Agentloft browser automation helpers.
//
// This file is the body of an async function. The host prepends
// `const ARGS = {...};` and appends `return JSON.stringify(await <helper>(ARGS));`,
// then runs it in an isolated world named "agentloft" (a DevTools isolated
// world in the daemon's browser): page scripts can neither see nor patch
// these helpers or the built-ins they use. Nothing is left in the page:
// refs live in this world (a WeakMap from element to number), not in the
// DOM, so the page sees no attributes, globals or mutations of ours.

const MAX_BLOCKS = 3000;
const MAX_COLLECTED_CHARS = 150000;
const INTERACTIVE =
  'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],' +
  '[role=checkbox],[role=radio],[role=switch],[role=tab],[role=menuitem],[role=option],' +
  '[role=combobox],[role=textbox],[contenteditable=""],[contenteditable=true]';
const ARIA_WIDGETS = new Set([
  'button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'menuitemcheckbox',
  'menuitemradio', 'option', 'combobox', 'textbox', 'searchbox', 'slider', 'spinbutton', 'treeitem',
]);
const SKIP_TAGS = new Set([
  'script', 'style', 'noscript', 'template', 'head', 'meta', 'link', 'title', 'svg', 'canvas',
  'video', 'audio', 'object', 'embed', 'map', 'datalist', 'math', 'option', 'optgroup',
]);
const TEXT_INPUT_TYPES = new Set([
  'text', 'search', 'email', 'url', 'tel', 'password', 'number', '',
]);
// `autocomplete` tokens of fields whose values are never shown back.
const SECRET_AUTOCOMPLETE = new Set([
  'current-password', 'new-password', 'one-time-code', 'cc-number', 'cc-csc', 'cc-exp',
  'cc-exp-month', 'cc-exp-year',
]);
// What of a page's header a main-area outline keeps: its fields and buttons.
const HEADER_CONTROLS =
  'input:not([type=hidden]),textarea,select,button,[role=searchbox],[role=combobox],[role=button]';
// Interactive elements outside the main area are counted up to this.
const MAX_LEFT_OUT = 500;

const state = globalThis.__loftAgent ||
  (globalThis.__loftAgent = { nextRef: 1, refs: new Map(), ids: new WeakMap() });
// Another site's frames, by the number the host gave each (their <iframe>s).
if (!state.frames) state.frames = new WeakMap();
// In such a frame, refs count from the frame's number × 100000, so a ref
// says which frame it is in.
if (ARGS.refBase && state.nextRef <= ARGS.refBase) state.nextRef = ARGS.refBase + 1;

// -- refs ---------------------------------------------------------------------

function refFor(el) {
  let id = state.ids.get(el);
  if (!id) {
    id = state.nextRef++;
    state.ids.set(el, id);
    state.refs.set(id, new WeakRef(el));
  }
  return id;
}

function findRef(id) {
  const known = state.refs.get(id);
  const el = known && known.deref();
  return el && el.isConnected ? el : null;
}

// -- describing elements ---------------------------------------------------------

function squash(s) {
  return (s || '').replace(/\s+/g, ' ').trim();
}

function clip(s, n) {
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}

function textOf(node) {
  if (!node) return '';
  return squash(node.innerText !== undefined ? node.innerText : node.textContent);
}

function styleOf(el) {
  const view = el.ownerDocument.defaultView || window;
  return view.getComputedStyle(el);
}

function isTextField(el) {
  if (el.localName === 'textarea') return true;
  return el.localName === 'input' && TEXT_INPUT_TYPES.has((el.getAttribute('type') || '').toLowerCase());
}

function isFormField(el) {
  return ['input', 'textarea', 'select'].includes(el.localName);
}

function isEditable(el) {
  if (el.isContentEditable) return true;
  const role = (el.getAttribute('role') || '').toLowerCase();
  return role === 'textbox' || role === 'searchbox';
}

// A password, a one-time code or a card's number: its value is never shown.
function isSecret(el) {
  if (!el || el.nodeType !== 1) return false;
  if (el.localName === 'input' && String(el.type || '').toLowerCase() === 'password') return true;
  const tokens = String(el.getAttribute('autocomplete') || '').toLowerCase().split(/\s+/);
  return tokens.some((token) => SECRET_AUTOCOMPLETE.has(token));
}

function roleOf(el, style) {
  const tag = el.localName;
  switch (tag) {
    case 'a':
      if (el.hasAttribute('href')) return 'link';
      break;
    case 'button':
      return 'button';
    case 'select':
      return 'select';
    case 'textarea':
      return 'textarea';
    case 'summary':
      return 'button';
    case 'input': {
      const type = (el.type || 'text').toLowerCase();
      if (type === 'hidden') return null;
      if (type === 'checkbox' || type === 'radio') return type;
      if (['submit', 'button', 'reset', 'image'].includes(type)) return 'button';
      if (type === 'range') return 'slider';
      return 'input';
    }
    case 'label':
    case 'html':
    case 'body':
      return null;
  }
  const aria = (el.getAttribute('role') || '').trim().split(/\s+/)[0].toLowerCase();
  if (ARIA_WIDGETS.has(aria)) {
    // A widget wrapping real controls: describe the controls instead.
    if (el.querySelector(INTERACTIVE)) return null;
    return aria === 'textbox' || aria === 'searchbox' ? 'editable' : aria;
  }
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'editable';
  if (aria === 'presentation' || aria === 'none') return null;
  // Script-driven controls: an onclick attribute, a keyboard-focusable
  // element, or the outermost element with a pointer cursor. Only short,
  // leaf-like ones, so whole cards and code views stay readable.
  const tabindex = el.getAttribute('tabindex');
  const pointer =
    style.cursor === 'pointer' && !(el.parentElement && styleOf(el.parentElement).cursor === 'pointer');
  if (el.hasAttribute('onclick') || (tabindex !== null && Number(tabindex) >= 0) || pointer) {
    if (el.querySelector(INTERACTIVE)) return null;
    const text = textOf(el);
    const named = text || el.getAttribute('aria-label') || el.getAttribute('title');
    if (named && text.length <= 120) return 'clickable';
  }
  return null;
}

// A label's own words, without those of the controls in it (a select's
// options, a field's text).
function labelText(label) {
  if (!label.querySelector('select,textarea,button,input')) return textOf(label);
  const words = [];
  const walker = label.ownerDocument.createTreeWalker(label, 4);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!node.parentElement.closest('select,textarea,button')) words.push(node.data);
  }
  return squash(words.join(' '));
}

function accessibleName(el) {
  const labelledBy = el.getAttribute('aria-labelledby');
  if (labelledBy) {
    const doc = el.ownerDocument;
    const text = squash(
      labelledBy.split(/\s+/).map((id) => textOf(doc.getElementById(id))).join(' '),
    );
    if (text) return text;
  }
  const aria = squash(el.getAttribute('aria-label'));
  if (aria) return aria;
  const tag = el.localName;
  if (tag === 'input') {
    const type = (el.type || 'text').toLowerCase();
    if (type === 'submit' || type === 'reset' || type === 'button') {
      return el.value || (type === 'submit' ? 'Submit' : type === 'reset' ? 'Reset' : '');
    }
    if (type === 'image') return el.alt || el.value || 'Submit';
  }
  if (isFormField(el)) {
    if (el.labels && el.labels.length) {
      const text = squash([...el.labels].map(labelText).join(' '));
      if (text) return text;
    }
    return squash(el.getAttribute('title'));
  }
  // What was typed into an editor is not its name.
  if (isEditable(el)) {
    return squash(
      el.getAttribute('aria-placeholder') || el.getAttribute('data-placeholder') ||
      el.getAttribute('placeholder') || el.getAttribute('title'),
    );
  }
  let text = textOf(el);
  if (!text) {
    const img = el.querySelector('img[alt]');
    if (img) text = squash(img.getAttribute('alt'));
  }
  if (!text) {
    const named = el.querySelector('[aria-label],[title],svg title');
    if (named) {
      text = squash(named.getAttribute && (named.getAttribute('aria-label') || named.getAttribute('title')))
        || squash(named.textContent);
    }
  }
  if (!text) text = squash(el.getAttribute('title'));
  return text;
}

// What a field holds, as a result may show it: secrets as their length only.
function shownValue(el, value) {
  if (!value) return '';
  if (isSecret(el)) return '[hidden: ' + value.length + ' characters]';
  return clip(value, 200);
}

// Values typed into fields are the user's (or the agent's) own, not the
// page's: a snapshot says a field is filled, and shows the value only of a
// read-only field that holds no secret.
function fieldValue(el, info) {
  const value = el.value || '';
  if (!value) return;
  if (el.readOnly && !isSecret(el)) info.value = clip(value, 200);
  else info.filled = true;
}

function describe(el, role) {
  const info = { ref: refFor(el), role, name: clip(accessibleName(el), 200) };
  const tag = el.localName;
  if (role === 'link' && tag === 'a') {
    const href = el.getAttribute('href');
    if (href !== null) info.href = el.href || href;
  }
  if (tag === 'input') {
    const type = (el.type || 'text').toLowerCase();
    if (role === 'input') {
      info.type = type;
      if (el.placeholder) info.placeholder = clip(squash(el.placeholder), 120);
      fieldValue(el, info);
    } else if (role === 'slider') {
      info.type = type;
      if (el.value) info.value = clip(el.value, 40);
    }
    if (role === 'checkbox' || role === 'radio') info.checked = !!el.checked;
    if (el.name) info.field = el.name;
  } else if (tag === 'textarea') {
    if (el.placeholder) info.placeholder = clip(squash(el.placeholder), 120);
    fieldValue(el, info);
    if (el.name) info.field = el.name;
  } else if (tag === 'select') {
    const options = [...el.options];
    info.optionCount = options.length;
    info.options = options.slice(0, 25).map((o) => clip(squash(o.label || o.text), 80));
    const selected = [...el.selectedOptions].map((o) => squash(o.label || o.text)).join(', ');
    if (selected) info.value = clip(selected, 200);
    if (el.name) info.field = el.name;
  } else {
    const checked = el.getAttribute('aria-checked');
    if (checked === 'true' || checked === 'false') info.checked = checked === 'true';
    if (role === 'editable' && textOf(el)) info.filled = true;
  }
  if (el.getAttribute('aria-selected') === 'true') info.selected = true;
  const expanded = el.getAttribute('aria-expanded');
  if (expanded === 'true' || expanded === 'false') info.expanded = expanded === 'true';
  if (el.disabled || el.getAttribute('aria-disabled') === 'true') info.disabled = true;
  return info;
}

function shortDescription(el) {
  if (!el || el.nodeType !== 1) return 'nothing';
  const role = roleOf(el, styleOf(el)) || el.localName;
  const name = clip(accessibleName(el), 60);
  return name ? role + ' "' + name + '"' : role;
}

// -- snapshot -------------------------------------------------------------------

function isHidden(el, style) {
  if (style.visibility === 'hidden' || style.visibility === 'collapse') return true;
  if (el.getAttribute('aria-hidden') === 'true') return true;
  if (el.getClientRects().length === 0) return true;
  const r = el.getBoundingClientRect();
  const view = el.ownerDocument.defaultView || window;
  // Screen-reader-only text and content parked far off-screen.
  if (r.width <= 1 && r.height <= 1 && style.overflow === 'hidden') return true;
  if (r.right + view.scrollX <= 0 || r.bottom + view.scrollY <= 0) return true;
  return false;
}

function isVisible(el) {
  if (!el || !el.isConnected) return false;
  const style = styleOf(el);
  if (style.display === 'none') return false;
  return !isHidden(el, style);
}

function isBlockDisplay(display) {
  return !(
    display.startsWith('inline') || display === 'contents' || display === 'table-cell' ||
    display.startsWith('ruby')
  );
}

function isRowFlex(style) {
  return (
    (style.display === 'flex' || style.display === 'inline-flex') &&
    !style.flexDirection.startsWith('column') && style.flexWrap === 'nowrap'
  );
}

function childrenOf(node) {
  if (node.shadowRoot) return node.shadowRoot.childNodes;
  if (node.localName === 'slot') {
    const assigned = node.assignedNodes({ flatten: true });
    return assigned.length ? assigned : node.childNodes;
  }
  return node.childNodes;
}

function hostOf(src) {
  try { return new URL(src, location.href).host; } catch (_) { return ''; }
}

// Where to start the outline, and what to say about what was left out.
// Asked for: a selector (or a ref number) to outline only that, or "page".
// Unasked: the page's main area when it has exactly one, holding most of
// the page's text, no modal dialog is open, and something is left out.
function snapshotScope(within) {
  const body = document.body;
  const result = { root: body, notes: [], header: [] };
  if (within && within !== 'page' && within !== 'body') {
    const target = /^\d+$/.test(within.trim()) ? { ref: Number(within) } : { selector: within };
    const found = resolve(target);
    if (found.el) {
      result.root = found.el;
      result.notes.push('Only ' + shortDescription(found.el) + ' (within ' + JSON.stringify(within) +
        ') is shown; snapshot without within for the whole page.');
    } else {
      result.notes.push('Nothing matches within ' + JSON.stringify(within) + ' (' + found.error.error +
        '); the whole page is shown.');
    }
    return result;
  }
  if (within || !body) return result;
  return scopeToMainArea(result);
}

// Whether `node` is `ancestor` or inside it, through shadow roots.
function isInside(node, ancestor) {
  return reaches(node, ancestor);
}

// The outline of a page's main area alone, when that is where the page's
// content is: no modal open, exactly one visible main area holding at
// least a quarter of the text, and something interactive left outside it
// (navigation, sidebars, a footer). The page header's fields and buttons
// are kept; a note says how much was left out.
function scopeToMainArea(result) {
  const body = result.root;
  const modal = queryDeep(document, 'dialog[open],[role=dialog][aria-modal=true],[role=alertdialog]');
  if (modal.some(isVisible)) return result;

  const candidates = queryDeep(document, 'main,[role=main]').filter(isVisible);
  const mains = candidates.filter((el) => !candidates.some((other) => other !== el && isInside(el, other)));
  if (mains.length !== 1) return result;
  const main = mains[0];
  if (textOf(main).length * 4 < textOf(body).length) return result;

  const header = [];
  for (const banner of queryDeep(document, 'header,[role=banner]')) {
    if (isInside(banner, main) || isInside(main, banner) || !isVisible(banner)) continue;
    for (const el of queryDeep(banner, HEADER_CONTROLS)) {
      if (header.includes(el) || el.closest('nav,[role=navigation]')) continue;
      if (isVisible(el)) header.push(el);
    }
  }
  documentOrder(header);
  const kept = new Set(header);

  // A "skip to content" link: to the main area, something in it, or
  // something around it.
  const skipsToMain = (el) => {
    if (el.localName !== 'a') return false;
    const href = el.getAttribute('href') || '';
    if (!href.startsWith('#') || href.length < 2) return false;
    let id = href.slice(1);
    try { id = decodeURIComponent(id); } catch (_) { /* as written */ }
    const target = el.ownerDocument.getElementById(id);
    return !!target && (target === main || isInside(target, main) || isInside(main, target));
  };
  let leftOut = 0;
  for (const el of queryDeep(document, INTERACTIVE)) {
    if (kept.has(el) || isInside(el, main) || skipsToMain(el) || !isVisible(el)) continue;
    leftOut++;
    if (leftOut >= MAX_LEFT_OUT) break;
  }
  if (!leftOut) return result;

  const count = leftOut >= MAX_LEFT_OUT ? MAX_LEFT_OUT + '+' : String(leftOut);
  const noun = leftOut === 1 ? 'interactive element' : 'interactive elements';
  const verb = leftOut === 1 ? 'is' : 'are';
  result.root = main;
  result.header = header;
  result.notes.push('Scoped to the page\'s main area: ' + count + ' ' + noun + ' outside it ' +
    '(navigation, sidebars, footer) ' + verb + ' not shown; take a snapshot with within "page" ' +
    'to include them.');
  return result;
}

function loftSnapshot(args) {
  const maxBlocks = Math.min(args.maxBlocks || MAX_BLOCKS, MAX_BLOCKS);
  const blocks = [];
  const frames = [];
  let current = null;
  let nextKind = null;
  let nextLevel = 0;
  let collected = 0;
  let truncated = false;
  const STOP = {};

  function flush() {
    if (current) {
      const parts = current.parts;
      while (parts.length && typeof parts[0] === 'string' && !parts[0].trim()) parts.shift();
      while (parts.length && typeof parts[parts.length - 1] === 'string' && !parts[parts.length - 1].trim()) parts.pop();
      if (parts.length) {
        for (let i = 0; i < parts.length; i++) if (typeof parts[i] === 'string') parts[i] = parts[i].trim();
        blocks.push(current);
        if (blocks.length >= maxBlocks) {
          truncated = true;
          current = null;
          throw STOP;
        }
      }
    }
    current = null;
  }
  function ensure() {
    if (!current) {
      current = { kind: nextKind || 'text', parts: [] };
      if (nextLevel) current.level = nextLevel;
      nextKind = null;
      nextLevel = 0;
    }
    return current;
  }
  function begin(kind, level) {
    flush();
    nextKind = kind;
    nextLevel = level || 0;
  }
  function count(n) {
    collected += n;
    if (collected > MAX_COLLECTED_CHARS) {
      truncated = true;
      throw STOP;
    }
  }
  function pushText(text) {
    text = text.replace(/\s+/g, ' ');
    if (!text.trim()) {
      if (current && current.parts.length) {
        const last = current.parts[current.parts.length - 1];
        if (typeof last === 'string' && !last.endsWith(' ')) current.parts[current.parts.length - 1] = last + ' ';
      }
      return;
    }
    count(text.length);
    const block = ensure();
    const last = block.parts[block.parts.length - 1];
    if (typeof last === 'string') block.parts[block.parts.length - 1] = last + text;
    else block.parts.push(text);
  }
  function pushElement(info) {
    count(32 + (info.name || '').length);
    ensure().parts.push(info);
  }

  function walk(node, ctx) {
    if (node.nodeType === 3) {
      if (!ctx.muteText) pushText(node.data);
      return;
    }
    if (node.nodeType === 11) {
      for (const child of node.childNodes) walk(child, ctx);
      return;
    }
    if (node.nodeType !== 1) return;
    const el = node;
    const tag = el.localName;
    if (SKIP_TAGS.has(tag)) return;
    if (ctx.skip && ctx.skip.has(el)) return;
    if (tag === 'br') {
      flush();
      return;
    }
    const style = styleOf(el);
    if (style.display === 'none') return;
    if (style.display === 'contents') {
      for (const child of childrenOf(el)) walk(child, ctx);
      return;
    }
    if (isHidden(el, style)) return;

    const role = roleOf(el, style);
    if (role) {
      pushElement(describe(el, role));
      return;
    }

    if (tag === 'iframe' || tag === 'frame') {
      let doc = null;
      try { doc = el.contentDocument; } catch (_) { /* cross-origin */ }
      flush();
      const title = squash(el.getAttribute('title') || el.getAttribute('name') || el.getAttribute('aria-label'));
      const src = el.getAttribute('src') || '';
      const host = src && hostOf(src) !== location.host ? hostOf(src) : '';
      const label = (title ? ' "' + clip(title, 60) + '"' : '') + (host ? ' (' + host + ')' : '');
      if (doc && doc.body) {
        blocks.push({ kind: 'text', parts: ['[frame' + label + ']'] });
        walk(doc.body, { muteText: false, shortCell: false });
        flush();
        blocks.push({ kind: 'text', parts: ['[end of frame]'] });
      } else {
        // Another site's frame: the host fills it in when it can reach it
        // (it numbered the frame).
        frames.push({ src: el.src || '', name: el.getAttribute('name') || '', title });
        const block = { kind: 'text', parts: ['[frame' + label + ']'] };
        if (state.frames.has(el)) block.frame = state.frames.get(el);
        blocks.push(block);
      }
      return;
    }
    if (tag === 'pre') {
      flush();
      const text = el.innerText || el.textContent || '';
      if (text.trim()) {
        count(Math.min(text.length, 2000));
        blocks.push({ kind: 'pre', parts: [text.slice(0, 2000)] });
      }
      return;
    }

    const headingMatch = /^h([1-6])$/.exec(tag);
    const ariaRole = (el.getAttribute('role') || '').toLowerCase();
    let kind = null;
    let level = 0;
    if (headingMatch) {
      kind = 'heading';
      level = Number(headingMatch[1]);
    } else if (ariaRole === 'heading') {
      kind = 'heading';
      level = Number(el.getAttribute('aria-level')) || 2;
    } else if (tag === 'li' || style.display === 'list-item') {
      kind = 'item';
    } else if (tag === 'blockquote') {
      kind = 'quote';
    }

    const display = style.display;
    const isTablePart = display.startsWith('table');
    const childCtx = {
      // Labels that name a control are read through that control.
      muteText: ctx.muteText || (tag === 'label' && !!el.control),
      // Inside a short table cell, block children stay on the row's line.
      shortCell: display === 'table-cell' ? textOf(el).length <= 200 : isTablePart ? false : ctx.shortCell,
      skip: ctx.skip,
    };
    // Short items of a single-row flex container (toolbars, button groups,
    // tab bars) stay on one line, as does the content of short table cells.
    const parentStyle = el.parentElement ? styleOf(el.parentElement) : null;
    const inline = !kind && !isTablePart &&
      (ctx.shortCell || (parentStyle && isRowFlex(parentStyle) && textOf(el).length <= 80));
    const block = !inline && (kind !== null || isBlockDisplay(display));

    if (kind) begin(kind, level);
    else if (block) flush();
    for (const child of childrenOf(el)) walk(child, childCtx);
    if (block) flush();
    else if (display === 'table-cell') pushText(' ');
  }

  const scope = snapshotScope(args.within);
  try {
    // The header's fields and buttons kept beside the main area: one line.
    if (scope.header.length) {
      begin('text');
      pushText('Header:');
      for (const el of scope.header) pushElement(describe(el, roleOf(el, styleOf(el)) || 'button'));
      flush();
    }
    if (scope.root) walk(scope.root, { muteText: false, shortCell: false, skip: new Set(scope.header) });
    flush();
  } catch (e) {
    if (e !== STOP) throw e;
  }

  const scroller = document.scrollingElement || document.documentElement;
  return {
    url: location.href,
    title: document.title,
    viewport: {
      width: window.innerWidth,
      height: window.innerHeight,
      scrollY: Math.round(scroller ? scroller.scrollTop : window.scrollY),
      scrollHeight: scroller ? scroller.scrollHeight : 0,
    },
    blocks,
    notes: scope.notes,
    frames,
    truncated,
  };
}

// -- targets ----------------------------------------------------------------------

function fail(code, error) {
  return { ok: false, code, error };
}

// Every element under `root` matching a CSS selector, open shadow roots
// included.
function queryDeep(root, css) {
  const out = [];
  const stack = [root];
  while (stack.length) {
    const scope = stack.pop();
    if (!scope.querySelectorAll) continue;
    for (const el of scope.querySelectorAll(css)) out.push(el);
    for (const el of scope.querySelectorAll('*')) if (el.shadowRoot) stack.push(el.shadowRoot);
  }
  return out;
}

function allElements(root) {
  return queryDeep(root, '*');
}

// `"Exact"` (quoted) or a case-insensitive part of the text.
function textMatcher(value) {
  const v = value.trim();
  const quoted = /^"(.*)"[is]?$/.exec(v) || /^'(.*)'[is]?$/.exec(v);
  if (quoted) {
    const want = squash(quoted[1]);
    return (s) => squash(s) === want;
  }
  const want = squash(v).toLowerCase();
  return (s) => squash(s).toLowerCase().includes(want);
}

// One step of a selector, from each element (or document) in `scopes`.
function selectStep(scopes, step) {
  const out = [];
  const add = (el) => { if (!out.includes(el)) out.push(el); };
  const eq = step.indexOf('=');
  const engine = eq > 0 && /^[a-z-]+$/.test(step.slice(0, eq)) ? step.slice(0, eq) : 'css';
  const value = engine === 'css' ? step : step.slice(eq + 1);
  for (const scope of scopes) {
    switch (engine) {
      case 'css':
        for (const el of queryDeep(scope, value)) add(el);
        break;
      case 'text': {
        const matches = textMatcher(value);
        const hits = allElements(scope).filter((el) => !SKIP_TAGS.has(el.localName) && matches(textOf(el)));
        // The innermost elements holding the text.
        for (const el of hits) if (!hits.some((other) => other !== el && el.contains(other))) add(el);
        break;
      }
      case 'label': {
        const matches = textMatcher(value);
        for (const el of allElements(scope)) {
          if ((isFormField(el) || isEditable(el)) && matches(accessibleName(el))) add(el);
        }
        break;
      }
      case 'placeholder': {
        const matches = textMatcher(value);
        for (const el of queryDeep(scope, '[placeholder],[aria-placeholder],[data-placeholder]')) {
          const p = el.getAttribute('placeholder') || el.getAttribute('aria-placeholder') || el.getAttribute('data-placeholder');
          if (matches(p)) add(el);
        }
        break;
      }
      case 'testid': {
        const want = value.trim().replace(/^["']|["']$/g, '');
        for (const el of queryDeep(scope, '[data-testid]')) if (el.getAttribute('data-testid') === want) add(el);
        break;
      }
      case 'role': {
        const m = /^([a-z]+)\s*(?:\[\s*name\s*=\s*(.+?)\s*\])?$/i.exec(value.trim());
        if (!m) throw new SyntaxError('role selectors look like role=button[name="Save"]');
        const role = m[1].toLowerCase();
        const matches = m[2] ? textMatcher(m[2]) : null;
        for (const el of allElements(scope)) {
          const own = (el.getAttribute('role') || '').toLowerCase().split(/\s+/)[0];
          const r = own || roleOf(el, styleOf(el));
          const same = r === role || (role === 'textbox' && (r === 'input' || r === 'textarea' || r === 'editable')) ||
            (role === 'combobox' && r === 'select');
          if (same && (!matches || matches(accessibleName(el)))) add(el);
        }
        break;
      }
      default:
        throw new SyntaxError('unknown selector engine "' + engine + '" (use CSS, text=, label=, placeholder=, role=, testid=)');
    }
  }
  return out;
}

// All the elements a selector matches: steps chained with `>>`, `frame`
// entering iframes (this site's own), `nth=N` picking one of the matches.
function selectAll(selector) {
  let scopes = [document];
  for (const raw of selector.split('>>')) {
    const step = raw.trim();
    if (!step) continue;
    if (step === 'frame' || step === 'internal:control=enter-frame') {
      const docs = [];
      for (const el of scopes) {
        let doc = null;
        try { doc = el.contentDocument; } catch (_) { /* cross-origin */ }
        if (!doc) {
          throw new Error(shortDescription(el) + ' is not a frame of this site; use a ref from the snapshot or click a point');
        }
        docs.push(doc);
      }
      scopes = docs;
      continue;
    }
    const nth = /^nth=(-?\d+)$/.exec(step);
    if (nth) {
      const n = Number(nth[1]);
      const pick = scopes[n < 0 ? scopes.length + n : n];
      scopes = pick ? [pick] : [];
      continue;
    }
    scopes = selectStep(scopes, step);
  }
  return scopes.filter((el) => el && el.nodeType === 1);
}

// The element a target names: `{ref}` or `{selector}` (which must match one,
// or one visible).
function resolve(target) {
  if (!target) return { error: fail('no_target', 'no element given') };
  if (target.ref) {
    const el = findRef(target.ref);
    if (!el) return { error: fail('ref_not_found', 'no element with [ref=' + target.ref + ']') };
    return { el };
  }
  const selector = String(target.selector || '').trim();
  if (!selector) return { error: fail('no_target', 'no element given') };
  let found;
  try {
    found = selectAll(selector);
  } catch (e) {
    return { error: fail('bad_selector', 'selector ' + JSON.stringify(selector) + ': ' + (e && e.message || e)) };
  }
  if (found.length > 1) {
    const visible = found.filter(isVisible);
    if (visible.length === 1) found = visible;
  }
  if (found.length === 0) return { error: fail('no_match', 'nothing matches selector ' + JSON.stringify(selector)) };
  if (found.length > 1) {
    const shown = found.slice(0, 5).map(shortDescription).join(', ');
    return {
      error: fail('ambiguous', 'selector ' + JSON.stringify(selector) + ' matches ' + found.length +
        ' elements (' + shown + (found.length > 5 ? ', …' : '') + '); make it more specific or add >> nth=0'),
    };
  }
  return { el: found[0] };
}

function loftDescribe(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  return { ok: true, element: shortDescription(el) };
}

// -- pointing -------------------------------------------------------------------

function isDisabled(el) {
  return !!el.disabled || el.getAttribute('aria-disabled') === 'true' || !!el.closest('fieldset[disabled]');
}

// The element's box in the top document's viewport, through the frames of
// this site it is in; null when one of them is another site's.
function topRect(el) {
  const r = el.getBoundingClientRect();
  let x = r.left;
  let y = r.top;
  let win = el.ownerDocument.defaultView;
  while (win && win !== window) {
    const frame = win.frameElement;
    if (!frame) return null;
    const fr = frame.getBoundingClientRect();
    const fs = styleOf(frame);
    x += fr.left + frame.clientLeft + (parseFloat(fs.paddingLeft) || 0);
    y += fr.top + frame.clientTop + (parseFloat(fs.paddingTop) || 0);
    win = frame.ownerDocument.defaultView;
  }
  return { left: x, top: y, width: r.width, height: r.height };
}

function inViewport(rect) {
  return rect.left + rect.width / 2 >= 0 && rect.top + rect.height / 2 >= 0 &&
    rect.left + rect.width / 2 < window.innerWidth && rect.top + rect.height / 2 < window.innerHeight;
}

// What a press at (x, y) of `doc`'s viewport lands on, shadow roots included.
function hitAt(doc, x, y) {
  let hit = doc.elementFromPoint(x, y);
  while (hit && hit.shadowRoot) {
    const inner = hit.shadowRoot.elementFromPoint(x, y);
    if (!inner || inner === hit) break;
    hit = inner;
  }
  return hit;
}

function reaches(hit, el) {
  for (let n = hit; n; n = n.parentNode || n.host) if (n === el) return true;
  return false;
}

// Where real input aimed at the element would land: its centre (or
// `offset` from its top-left corner), scrolled into view, in the top
// document's viewport. `covered` names what is on top there instead;
// `synthetic` says why only a dispatched event can reach it.
function loftAim(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  if (args.enabled !== false && isDisabled(el)) return fail('disabled', shortDescription(el) + ' is disabled');
  const element = shortDescription(el);
  let rect = topRect(el);
  if (!rect) return { ok: true, element, synthetic: 'in another site\'s frame' };
  if (!inViewport(rect)) {
    el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    rect = topRect(el);
  }
  if (rect.width < 1 || rect.height < 1) return { ok: true, element, synthetic: 'no size' };
  const offset = args.offset;
  const lx = offset ? offset.x : rect.width / 2;
  const ly = offset ? offset.y : rect.height / 2;
  const x = rect.left + lx;
  const y = rect.top + ly;
  if (x < 0 || y < 0 || x >= window.innerWidth || y >= window.innerHeight) {
    return { ok: true, element, synthetic: 'outside the viewport' };
  }
  // Hit-test in the element's own document, then each frame on the way up.
  const own = el.getBoundingClientRect();
  const hit = hitAt(el.ownerDocument, own.left + lx, own.top + ly);
  let cover = hit && !reaches(hit, el) ? hit : null;
  for (let win = el.ownerDocument.defaultView; !cover && win !== window; win = win.frameElement.ownerDocument.defaultView) {
    const frame = win.frameElement;
    const top = topRect(frame);
    const fr = frame.getBoundingClientRect();
    const fh = hitAt(frame.ownerDocument, fr.left + (x - top.left), fr.top + (y - top.top));
    if (fh && !reaches(fh, frame)) cover = fh;
  }
  const result = { ok: true, element, x, y };
  if (cover) result.covered = shortDescription(cover);
  return result;
}

// Where the content of the frame numbered `args.frame` starts in the
// viewport (its <iframe> scrolled into view first), and what of this
// document covers `args.point` of the frame, if anything.
function loftFrameOffset(args) {
  const frame = queryDeep(document, 'iframe,frame').find((el) => state.frames.get(el) === args.frame);
  if (!frame) return fail('frame_gone', 'the frame is no longer on the page; take a new snapshot');
  let rect = topRect(frame);
  if (!rect) return fail('frame_unreachable', 'the frame is inside another site\'s frame');
  if (!inViewport(rect)) {
    frame.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    rect = topRect(frame);
  }
  const fs = styleOf(frame);
  const x = rect.left + frame.clientLeft + (parseFloat(fs.paddingLeft) || 0);
  const y = rect.top + frame.clientTop + (parseFloat(fs.paddingTop) || 0);
  const result = { ok: true, x, y, element: shortDescription(frame) };
  if (args.point) {
    const px = x + args.point.x;
    const py = y + args.point.y;
    if (px < 0 || py < 0 || px >= window.innerWidth || py >= window.innerHeight) {
      result.outside = true;
    } else {
      const hit = hitAt(document, px, py);
      if (hit && !reaches(hit, frame)) result.covered = shortDescription(hit);
    }
  }
  return result;
}

function mouse(el, type, x, y, buttons) {
  const view = el.ownerDocument.defaultView || window;
  const init = {
    bubbles: true, cancelable: true, composed: true, view, clientX: x, clientY: y,
    screenX: x + view.screenX, screenY: y + view.screenY, button: 0, buttons, detail: 1,
  };
  if (type.startsWith('pointer') && typeof view.PointerEvent === 'function') {
    return el.dispatchEvent(new view.PointerEvent(type, {
      ...init, pointerId: 1, pointerType: 'mouse', isPrimary: true, width: 1, height: 1,
      pressure: buttons ? 0.5 : 0,
    }));
  }
  return el.dispatchEvent(new view.MouseEvent(type, init));
}

function deepActiveElement() {
  let el = document.activeElement;
  for (;;) {
    if (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    else if (el && el.localName === 'iframe') {
      let inner = null;
      try { inner = el.contentDocument && el.contentDocument.activeElement; } catch (_) { /* cross-origin */ }
      if (!inner || inner === el.contentDocument.body) break;
      el = inner;
    } else break;
  }
  return el || document.body;
}

function focusable(el) {
  return typeof el.focus === 'function' && (el.tabIndex >= 0 || el.isContentEditable || isFormField(el));
}

// A click made of dispatched events, for what real input can't reach (an
// element covered by another, or in another site's frame).
function loftClick(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  if (isDisabled(el)) return fail('disabled', shortDescription(el) + ' is disabled');
  const described = shortDescription(el);
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  const x = r.left + r.width / 2;
  const y = r.top + r.height / 2;
  const hit = el.ownerDocument.elementFromPoint(x, y);
  const target = hit && (hit === el || el.contains(hit)) ? hit : el;
  mouse(target, 'pointerover', x, y, 0);
  mouse(target, 'mouseover', x, y, 0);
  mouse(target, 'pointermove', x, y, 0);
  mouse(target, 'mousemove', x, y, 0);
  mouse(target, 'pointerdown', x, y, 1);
  const downOk = mouse(target, 'mousedown', x, y, 1);
  if (downOk && focusable(el) && deepActiveElement() !== el) el.focus({ preventScroll: true });
  mouse(target, 'pointerup', x, y, 0);
  mouse(target, 'mouseup', x, y, 0);
  mouse(target, 'click', x, y, 0);
  return { ok: true, element: described };
}

// -- typing ---------------------------------------------------------------------

function hasFocus(el) {
  const active = deepActiveElement();
  return active === el || (el.isContentEditable && el.contains(active));
}

// Focus the element for typing, its caret at the end (or everything in it
// selected, to type over it).
function loftFocus(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  if (isDisabled(el)) return fail('disabled', shortDescription(el) + ' is disabled');
  const element = shortDescription(el);
  if (!hasFocus(el)) el.focus({ preventScroll: false });
  if (!hasFocus(el)) {
    return fail('no_focus', element + ' did not take focus (it may be hidden, covered or not focusable), so nothing was typed');
  }
  const doc = el.ownerDocument;
  if (isTextField(el)) {
    const end = (el.value || '').length;
    try { el.setSelectionRange(args.select ? 0 : end, end); } catch (_) { if (args.select) el.select(); }
  } else if (el.isContentEditable) {
    const range = doc.createRange();
    range.selectNodeContents(el);
    if (!args.select) range.collapse(false);
    const selection = doc.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
  }
  return { ok: true, element };
}

// What `fill` has to do with the element: toggle it (`check`), pick an
// option (`select`), or type over it (`text`, focused with its content
// selected).
function loftFillKind(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  const element = shortDescription(el);
  const tag = el.localName;
  const type = (el.type || '').toLowerCase();
  if (tag === 'label' && el.control) return loftFillKind({ target: { ref: refFor(el.control) } });
  if ((tag === 'input' && (type === 'checkbox' || type === 'radio')) ||
      ['checkbox', 'radio', 'switch', 'menuitemcheckbox', 'menuitemradio'].includes((el.getAttribute('role') || '').toLowerCase())) {
    const checked = tag === 'input' ? !!el.checked : el.getAttribute('aria-checked') === 'true';
    return { ok: true, kind: 'check', element, checked, radio: type === 'radio' || el.getAttribute('role') === 'radio', ref: refFor(el) };
  }
  if (tag === 'select') return { ok: true, kind: 'select', element, ref: refFor(el) };
  if (tag === 'input' && type === 'file') return fail('not_editable', element + ' is a file input; use upload');
  if (tag === 'input' && ['submit', 'button', 'reset', 'image', 'range', 'color'].includes(type)) {
    return fail('not_editable', element + ' does not take text');
  }
  if (isDisabled(el)) return fail('disabled', element + ' is disabled');
  if ((tag === 'input' || tag === 'textarea') && el.readOnly) return fail('readonly', element + ' is read-only');
  if (tag !== 'input' && tag !== 'textarea' && !el.isContentEditable) {
    return fail('not_editable', element + ' is not a text field; pick an input, or click it and use type');
  }
  const focused = loftFocus({ target: { ref: refFor(el) }, select: true });
  if (!focused.ok) return focused;
  return { ok: true, kind: 'text', element, ref: refFor(el), empty: !(el.value || textOf(el)) };
}

// What the field holds now, as a result may show it.
function loftFieldValue(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  const value = el.isContentEditable ? textOf(el) : String(el.value || '');
  const expected = squash(String(args.expected || ''));
  return { ok: true, value: shownValue(el, value), matches: squash(value) === expected, secret: isSecret(el) };
}

// -- select -----------------------------------------------------------------------

const nativeSetters = new Map();
function nativeValueSetter(el) {
  const proto = Object.getPrototypeOf(el);
  if (!nativeSetters.has(proto)) {
    let p = proto;
    let setter = null;
    while (p && !setter) {
      const d = Object.getOwnPropertyDescriptor(p, 'value');
      if (d && d.set) setter = d.set;
      p = Object.getPrototypeOf(p);
    }
    nativeSetters.set(proto, setter);
  }
  return nativeSetters.get(proto);
}

function optionLabel(o) {
  return squash(o.label || o.text);
}

function findOption(options, wanted) {
  const text = String(wanted);
  const lower = squash(text).toLowerCase();
  return options.find((o) => o.value === text) ||
    options.find((o) => optionLabel(o) === squash(text)) ||
    options.find((o) => optionLabel(o).toLowerCase() === lower) ||
    options.find((o) => optionLabel(o).toLowerCase().includes(lower));
}

function loftSelect(args) {
  const { el, error } = resolve(args.target);
  if (error) return error;
  const element = shortDescription(el);
  if (el.localName !== 'select') {
    return fail('not_select', element + ' is not a <select>; for a custom dropdown, click it and then the option');
  }
  if (isDisabled(el)) return fail('disabled', element + ' is disabled');
  const values = args.values || [];
  if (!values.length) return fail('no_values', 'no option given');
  if (values.length > 1 && !el.multiple) return fail('not_multiple', element + ' takes one option');
  const options = [...el.options];
  const chosen = [];
  for (const wanted of values) {
    const option = findOption(options, wanted);
    if (!option) {
      const listed = options.slice(0, 30).map((o) => JSON.stringify(optionLabel(o)) +
        (o.value !== optionLabel(o) ? ' (value ' + JSON.stringify(o.value) + ')' : '')).join(', ');
      return fail('no_option', 'no option matching ' + JSON.stringify(String(wanted)) + ' in ' + element +
        (listed ? '; options: ' + listed : '; it has no options'));
    }
    if (option.disabled) return fail('disabled', 'the option ' + JSON.stringify(optionLabel(option)) + ' is disabled');
    chosen.push(option);
  }
  el.focus({ preventScroll: true });
  if (el.multiple) {
    for (const o of options) o.selected = chosen.includes(o);
  } else {
    const setter = nativeValueSetter(el);
    if (setter) setter.call(el, chosen[0].value);
    else el.value = chosen[0].value;
  }
  el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true, element, selected: [...el.selectedOptions].map(optionLabel) };
}

// -- scrolling --------------------------------------------------------------------

function scrollsItself(el) {
  const s = styleOf(el);
  return (/(auto|scroll|overlay)/.test(s.overflowY) && el.scrollHeight > el.clientHeight + 1) ||
    (/(auto|scroll|overlay)/.test(s.overflowX) && el.scrollWidth > el.clientWidth + 1);
}

function scroller() {
  return document.scrollingElement || document.documentElement;
}

function scrollableAround(el) {
  for (let n = el; n && n !== document.body && n !== document.documentElement; n = n.parentElement || (n.getRootNode && n.getRootNode().host)) {
    if (n.nodeType === 1 && scrollsItself(n)) return n;
  }
  return null;
}

function scrollState(box, name) {
  const page = box === scroller();
  const height = page ? window.innerHeight : box.clientHeight;
  const top = Math.round(box.scrollTop);
  return {
    ok: true,
    target: name,
    scroll_y: top,
    scroll_x: Math.round(box.scrollLeft),
    scroll_height: box.scrollHeight,
    viewport_height: height,
    at_top: top <= 0,
    at_bottom: top + height >= box.scrollHeight - 1,
  };
}

// Where to turn the wheel: over the target, or over the page somewhere
// nothing that scrolls by itself is (so the page itself scrolls).
function loftScrollPoint(args) {
  const page = scroller();
  const pageTop = page.scrollTop;
  if (args.target) {
    const aim = loftAim({ target: args.target, enabled: false });
    if (!aim.ok) return aim;
    if (aim.x === undefined) return fail('unreachable', aim.element + ' can\'t be scrolled with the wheel (' + aim.synthetic + ')');
    return { ...aim, pageTop: page.scrollTop };
  }
  const w = window.innerWidth;
  const h = window.innerHeight;
  const pageScrolls = page.scrollHeight > h + 1 || page.scrollWidth > w + 1;
  for (const [fx, fy] of [[0.5, 0.5], [0.1, 0.5], [0.9, 0.5], [0.5, 0.2], [0.5, 0.8], [0.02, 0.5], [0.98, 0.5]]) {
    const x = Math.round(w * fx);
    const y = Math.round(h * fy);
    const hit = hitAt(document, x, y);
    if (!pageScrolls || !hit || !scrollableAround(hit)) return { ok: true, x, y, pageTop };
  }
  return { ok: true, x: Math.round(w / 2), y: Math.round(h / 2), pageTop };
}

// How far what was scrolled is: the target when it scrolls itself, else
// what scrolls under (x, y), else the page.
function loftScrollState(args) {
  if (args.target) {
    const { el, error } = resolve(args.target);
    if (error) return error;
    const box = scrollsItself(el) ? el : scrollableAround(el);
    if (box) return scrollState(box, shortDescription(box));
  } else if (args.x !== undefined) {
    const hit = hitAt(document, args.x, args.y);
    const box = hit && scrollableAround(hit);
    const page = scroller();
    // The page when it scrolls (wheel turns reach it once a box is at its end).
    if (box && !(page.scrollHeight > window.innerHeight + 1 && page.scrollTop !== args.pageTop)) {
      return scrollState(box, shortDescription(box));
    }
  }
  return scrollState(scroller(), 'page');
}

// To an edge (of the target, or the page), or the target into view.
function loftScrollTo(args) {
  let box = scroller();
  let name = 'page';
  if (args.target) {
    const { el, error } = resolve(args.target);
    if (error) return error;
    if (!args.to) {
      el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      const around = scrollableAround(el);
      return { ...(around ? scrollState(around, shortDescription(around)) : scrollState(scroller(), 'page')), element: shortDescription(el) };
    }
    box = scrollsItself(el) ? el : (scrollableAround(el) || box);
    name = box === scroller() ? 'page' : shortDescription(box);
  }
  const options = { behavior: 'instant' };
  if (args.to === 'top') options.top = 0;
  if (args.to === 'bottom') options.top = box.scrollHeight;
  if (args.to === 'left') options.left = 0;
  if (args.to === 'right') options.left = box.scrollWidth;
  if (box === scroller()) window.scrollTo(options);
  else box.scrollTo(options);
  return scrollState(box, name);
}

// -- waiting ------------------------------------------------------------------------

function pageText(doc, depth) {
  let text = doc.body ? (doc.body.innerText || '') : '';
  if (depth < 3) {
    for (const frame of doc.querySelectorAll('iframe,frame')) {
      let inner = null;
      try { inner = frame.contentDocument; } catch (_) { /* cross-origin */ }
      if (inner) text += '\n' + pageText(inner, depth + 1);
    }
  }
  return text;
}

// An address pattern: `/regex/flags`, a glob with `*` (`**` crosses `/`),
// or a part of the address.
function urlMatches(pattern, url) {
  const p = String(pattern);
  const re = /^\/(.+)\/([a-z]*)$/.exec(p);
  if (re) {
    try { return new RegExp(re[1], re[2]).test(url); } catch (_) { /* not a regex: a path */ }
  }
  if (p.includes('*')) {
    let source = '';
    for (let i = 0; i < p.length; i++) {
      const c = p[i];
      if (c === '*' && p[i + 1] === '*') { source += '.*'; i++; } else if (c === '*') source += '[^/]*';
      else source += c.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
    }
    return new RegExp('^' + source + '$').test(url) || new RegExp(source).test(url);
  }
  return url.includes(p);
}

// Which of the conditions hold now.
function loftWait(args) {
  const unmet = [];
  if (args.text || args.textGone) {
    const text = squash(pageText(document, 0));
    const has = (s) => args.exact ? text.includes(squash(s)) : text.toLowerCase().includes(squash(s).toLowerCase());
    if (args.text && !has(args.text)) unmet.push('the text ' + JSON.stringify(args.text) + ' to appear');
    if (args.textGone && has(args.textGone)) unmet.push('the text ' + JSON.stringify(args.textGone) + ' to go');
  }
  if (args.target) {
    const want = args.state || 'visible';
    const { el, error } = resolve(args.target);
    if (error && error.code === 'bad_selector') return error;
    if (error && error.code === 'ambiguous' && want !== 'detached' && want !== 'hidden') return error;
    const attached = !!el;
    const visible = attached && isVisible(el);
    const holds = want === 'attached' ? attached : want === 'detached' ? !attached :
      want === 'hidden' ? !visible : visible;
    if (!holds) unmet.push((args.target.ref ? '[ref=' + args.target.ref + ']' : JSON.stringify(args.target.selector)) + ' to be ' + want);
  }
  if (args.url && !urlMatches(args.url, location.href)) unmet.push('the address to match ' + JSON.stringify(args.url));
  return { ok: true, met: unmet.length === 0, unmet, url: location.href };
}

// -- challenges ---------------------------------------------------------------------

// What tells a bot check or a block page from the site (the host decides,
// `challenge.rs`): the address, title, visible text, the addresses of
// frames and scripts, and whether the elements `args.selectors` (and ids
// starting with `args.idPrefixes`) are there, and have a box.
function loftProbe(args) {
  const text = document.body ? String(document.body.innerText || '') : '';
  const addresses = (css) => {
    const out = [];
    for (const el of document.querySelectorAll(css)) {
      const src = String(el.src || el.getAttribute('src') || '');
      if (!src) continue;
      out.push(src);
      if (out.length >= 50) break;
    }
    return out;
  };
  const look = (css) => {
    let el = null;
    try { el = document.querySelector(css); } catch (_) { /* not a selector here */ }
    if (!el) return { found: false, visible: false, src: '' };
    const r = el.getBoundingClientRect();
    return { found: true, visible: r.width > 0 && r.height > 0, src: String(el.src || el.getAttribute('src') || '') };
  };
  const elements = {};
  for (const css of args.selectors || []) elements[css] = look(css);
  for (const prefix of args.idPrefixes || []) elements['[id^=' + prefix + ']'] = look('[id^="' + CSS.escape(prefix) + '"]');
  return {
    ok: true,
    url: location.href,
    title: String(document.title || '').slice(0, 300),
    text: text.slice(0, 3000),
    textLength: text.length,
    iframes: addresses('iframe'),
    scripts: addresses('script[src]'),
    elements,
  };
}

// -- uploads --------------------------------------------------------------------------

// The file input a target names (itself, or the one its label is for), as
// the element itself (the host hands it files by its object id), or a
// string saying why not.
function loftFileInput(args) {
  const { el, error } = resolve(args.target);
  if (error) return error.error;
  let input = el;
  if (input.localName === 'label' && input.control) input = input.control;
  if (input.localName !== 'input' || (input.type || '').toLowerCase() !== 'file') {
    const inner = input.querySelector && input.querySelector('input[type=file]');
    if (!inner) return shortDescription(el) + ' is not a file input (input type=file, or its label)';
    input = inner;
  }
  if (isDisabled(input)) return shortDescription(input) + ' is disabled';
  return input;
}

// -- locate (Scripts (D)) ------------------------------------------------------
//
// What a script's locators stand for (`page.locator('form').getByRole('button',
// { name: 'Save' })`): a chain of steps, each searching inside what the steps
// before it found, or narrowing it. The matches get refs like the snapshot's,
// so the commands that take a ref act on them.

const IMPLICIT_ROLES = {
  article: 'article', aside: 'complementary', button: 'button', dialog: 'dialog',
  datalist: 'listbox', details: 'group', fieldset: 'group', figure: 'figure', footer: 'contentinfo',
  form: 'form', h1: 'heading', h2: 'heading', h3: 'heading', h4: 'heading', h5: 'heading',
  h6: 'heading', header: 'banner', hr: 'separator', img: 'img', li: 'listitem', main: 'main',
  menu: 'list', meter: 'meter', nav: 'navigation', ol: 'list', optgroup: 'group', option: 'option',
  output: 'status', progress: 'progressbar', summary: 'button', table: 'table', tbody: 'rowgroup',
  td: 'cell', textarea: 'textbox', tfoot: 'rowgroup', th: 'columnheader', thead: 'rowgroup',
  tr: 'row', ul: 'list',
};
const INPUT_ROLES = {
  button: 'button', checkbox: 'checkbox', email: 'textbox', image: 'button', number: 'spinbutton',
  radio: 'radio', range: 'slider', reset: 'button', search: 'searchbox', submit: 'button',
  tel: 'textbox', text: 'textbox', url: 'textbox', password: 'textbox',
};

/** The element's ARIA role: its own `role`, else the one its tag implies. */
function ariaRole(el) {
  const own = (el.getAttribute('role') || '').trim().split(/\s+/)[0].toLowerCase();
  if (own) return own;
  const tag = el.localName;
  if (tag === 'a' || tag === 'area') return el.hasAttribute('href') ? 'link' : null;
  if (tag === 'input') {
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    if (type === 'hidden') return null;
    if (el.hasAttribute('list') && ['text', 'search', 'email', 'url', 'tel'].includes(type)) return 'combobox';
    return INPUT_ROLES[type] || 'textbox';
  }
  if (tag === 'select') return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
  if (tag === 'section') {
    return el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby') ? 'region' : null;
  }
  if (tag === 'img' && el.getAttribute('alt') === '') return 'presentation';
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
  return IMPLICIT_ROLES[tag] || null;
}

/** The elements below `scope`, open shadow roots and same-origin frames included. */
function* deepElements(scope) {
  const stack = [scope];
  if (scope.shadowRoot) stack.push(scope.shadowRoot);
  while (stack.length) {
    const node = stack.shift();
    if (!node.querySelectorAll) continue;
    for (const el of node.querySelectorAll('*')) {
      yield el;
      if (el.shadowRoot) stack.push(el.shadowRoot);
      if (el.localName === 'iframe') {
        try { if (el.contentDocument) stack.push(el.contentDocument); } catch (_) { /* cross-origin */ }
      }
    }
  }
}

/** A test for `step[key]`: a substring whatever the case, the whole text (`exact`), or a regex. */
function locateMatcher(step, key) {
  const wanted = step[key];
  if (wanted === undefined || wanted === null) return () => true;
  if (step.regex !== undefined && step.regex !== null && step.regex !== false) {
    const re = new RegExp(String(wanted), typeof step.regex === 'string' ? step.regex : '');
    return (text) => re.test(text || '');
  }
  const plain = squash(String(wanted));
  if (step.exact) return (text) => squash(text) === plain;
  const lower = plain.toLowerCase();
  return (text) => squash(text).toLowerCase().includes(lower);
}

function locateVisible(el) {
  return !isHidden(el, styleOf(el));
}

function checkedState(el) {
  if (el.localName === 'input' && ['checkbox', 'radio'].includes((el.type || '').toLowerCase())) return !!el.checked;
  return el.getAttribute('aria-checked') === 'true';
}

function matchesStep(el, step) {
  if (step.role !== undefined) {
    if (ariaRole(el) !== String(step.role).toLowerCase()) return false;
    if (!locateMatcher(step, 'name')(accessibleName(el))) return false;
    if (step.level !== undefined) {
      const level = (el.localName.match(/^h(\d)$/) || [])[1] || el.getAttribute('aria-level');
      if (Number(level) !== Number(step.level)) return false;
    }
    if (step.checked !== undefined && checkedState(el) !== !!step.checked) return false;
    return true;
  }
  if (step.text !== undefined) {
    if (SKIP_TAGS.has(el.localName)) return false;
    const match = locateMatcher(step, 'text');
    const own = el.localName === 'input' && ['submit', 'button', 'reset'].includes((el.type || '').toLowerCase())
      ? el.value
      : textOf(el);
    // The innermost elements whose text matches, as a person would point at.
    return match(own) && ![...el.children].some((child) => match(textOf(child)));
  }
  if (step.label !== undefined) {
    const labelled = isFormField(el) || el.isContentEditable ||
      el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby');
    return labelled && locateMatcher(step, 'label')(accessibleName(el));
  }
  const attribute = step.placeholder !== undefined ? ['placeholder', 'placeholder']
    : step.alt !== undefined ? ['alt', 'alt']
    : step.title !== undefined ? ['title', 'title']
    : step.testid !== undefined ? ['data-testid', 'testid']
    : null;
  if (!attribute) throw new Error('unknown locator step ' + JSON.stringify(step));
  return el.hasAttribute(attribute[0]) && locateMatcher(step, attribute[1])(el.getAttribute(attribute[0]));
}

function searchStep(scopes, step) {
  const found = new Set();
  for (const scope of scopes) {
    if (step.css !== undefined) {
      const roots = [scope];
      for (const el of deepElements(scope)) if (el.shadowRoot) roots.push(el.shadowRoot);
      for (const root of roots) {
        let list;
        try {
          list = root.querySelectorAll(String(step.css));
        } catch (_) {
          throw new Error('not a CSS selector: ' + step.css);
        }
        for (const el of list) found.add(el);
      }
      continue;
    }
    for (const el of deepElements(scope)) {
      if (!matchesStep(el, step)) continue;
      // Like a person (and Playwright), hidden elements don't count unless asked.
      if (step.include_hidden !== true && !locateVisible(el)) continue;
      found.add(el);
    }
  }
  return [...found];
}

/** A narrowing step's result, or null for a step that searches. */
function narrowStep(found, step) {
  if (step.nth !== undefined) {
    const n = Number(step.nth);
    const el = n < 0 ? found[found.length + n] : found[n];
    return el ? [el] : [];
  }
  const textOrValue = (el) => textOf(el) || el.value || '';
  if (step.has_text !== undefined) {
    const match = locateMatcher({ ...step, text: step.has_text }, 'text');
    return found.filter((el) => match(textOrValue(el)));
  }
  if (step.has_not_text !== undefined) {
    const match = locateMatcher({ ...step, text: step.has_not_text }, 'text');
    return found.filter((el) => !match(textOrValue(el)));
  }
  if (step.visible !== undefined) return found.filter((el) => locateVisible(el) === !!step.visible);
  return null;
}

function documentOrder(list) {
  return list.sort((a, b) => {
    if (a === b) return 0;
    const position = a.compareDocumentPosition(b);
    if (position & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
    if (position & Node.DOCUMENT_POSITION_PRECEDING) return 1;
    return 0;
  });
}

function loftLocate(args) {
  const steps = Array.isArray(args.steps) ? args.steps : [];
  if (!steps.length) return fail('bad_locator', 'a locator needs at least one step');
  const limit = Math.max(0, Math.min(Number(args.limit ?? 20) || 0, 500));
  let found = [document];
  let searched = false;
  try {
    for (const step of steps) {
      if (!step || typeof step !== 'object') return fail('bad_locator', 'a locator step must be an object');
      const narrowed = searched ? narrowStep(found, step) : null;
      if (narrowed) {
        found = narrowed;
        continue;
      }
      found = documentOrder(searchStep(found, step));
      searched = true;
    }
  } catch (e) {
    return fail('bad_locator', String((e && e.message) || e));
  }
  const shown = found.slice(0, limit);
  return { ok: true, count: found.length, refs: shown.map(refFor), elements: shown.map(shortDescription) };
}

// The elements `args.refs` stand for, as themselves (not JSON), for a
// function the agent's script runs on them in the page's own world
// (`eval_on`); a string says which one is gone.
function loftElements(args) {
  const out = [];
  for (const id of args.refs || []) {
    const el = findRef(id);
    if (!el) return 'the element [ref=' + id + '] is no longer on the page; locate it again';
    out.push(el);
  }
  return out;
}
