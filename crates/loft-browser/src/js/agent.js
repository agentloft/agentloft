// Agentloft browser automation helpers.
//
// This file is the body of an async function. The host prepends
// `const ARGS = {...};` and appends `return JSON.stringify(<helper>(ARGS));`,
// then runs it in an isolated WKContentWorld named "agentloft": page scripts
// can neither see nor patch these helpers or the built-ins they use, while
// the DOM (including the `data-loft-ref` attributes set here) is shared.

const REF_ATTR = 'data-loft-ref';
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

const state = globalThis.__loftAgent || (globalThis.__loftAgent = { nextRef: 0, refs: new Map() });
if (!state.nextRef) {
  // A fresh world (new document): continue numbering after refs already in
  // the DOM, e.g. on a page restored from the back/forward cache.
  let max = 0;
  for (const el of elementsWithRefs(document)) max = Math.max(max, Number(el.getAttribute(REF_ATTR)) || 0);
  state.nextRef = max + 1;
}

// -- refs ---------------------------------------------------------------------

function refFor(el) {
  let id = Number(el.getAttribute(REF_ATTR)) || 0;
  const known = id ? state.refs.get(id) : null;
  const holder = known && known.deref();
  if (!id || (holder && holder !== el && holder.isConnected)) {
    // Unnumbered, or a clone that copied another element's attribute.
    id = state.nextRef++;
    el.setAttribute(REF_ATTR, String(id));
  }
  state.refs.set(id, new WeakRef(el));
  return id;
}

function* elementsWithRefs(root) {
  const stack = [root];
  while (stack.length) {
    const node = stack.pop();
    if (!node.querySelectorAll) continue;
    for (const el of node.querySelectorAll('[' + REF_ATTR + ']')) yield el;
    for (const el of node.querySelectorAll('*')) {
      if (el.shadowRoot) stack.push(el.shadowRoot);
      if (el.localName === 'iframe') {
        try { if (el.contentDocument) stack.push(el.contentDocument); } catch (_) { /* cross-origin */ }
      }
    }
  }
}

function findRef(id) {
  const known = state.refs.get(id);
  const el = known && known.deref();
  if (el && el.isConnected) return el;
  for (const candidate of elementsWithRefs(document)) {
    if (Number(candidate.getAttribute(REF_ATTR)) === id) {
      state.refs.set(id, new WeakRef(candidate));
      return candidate;
    }
  }
  return null;
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
      const text = squash([...el.labels].map(textOf).join(' '));
      if (text) return text;
    }
    return squash(el.getAttribute('title'));
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
  if (!text && el.isContentEditable) {
    text = squash(el.getAttribute('data-placeholder') || el.getAttribute('placeholder'));
  }
  return text;
}

function maskedValue(el) {
  const value = el.value || '';
  if ((el.type || '').toLowerCase() === 'password') return value ? '•'.repeat(Math.min(value.length, 8)) : '';
  return clip(value, 200);
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
    if (role === 'input' || role === 'slider') {
      info.type = type;
      if (el.placeholder) info.placeholder = clip(squash(el.placeholder), 120);
      const value = maskedValue(el);
      if (value) info.value = value;
    }
    if (role === 'checkbox' || role === 'radio') info.checked = !!el.checked;
    if (el.name) info.field = el.name;
  } else if (tag === 'textarea') {
    if (el.placeholder) info.placeholder = clip(squash(el.placeholder), 120);
    if (el.value) info.value = clip(el.value, 200);
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
    if (role === 'editable') {
      const value = textOf(el);
      if (value && value !== info.name) info.value = clip(value, 200);
    }
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

function loftSnapshot(args) {
  const maxBlocks = Math.min(args.maxBlocks || MAX_BLOCKS, MAX_BLOCKS);
  const blocks = [];
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
      if (doc && doc.body) {
        walk(doc.body, { muteText: false, shortCell: false });
      } else {
        const title = squash(el.getAttribute('title') || el.getAttribute('name'));
        pushText('[cross-origin frame' + (title ? ': ' + title : '') + ']');
      }
      flush();
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

  try {
    if (document.body) walk(document.body, { muteText: false, shortCell: false });
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
    truncated,
  };
}

// -- actions --------------------------------------------------------------------

function fail(code, error) {
  return { ok: false, code, error };
}

function lookup(id) {
  const el = findRef(id);
  if (!el) return { error: fail('ref_not_found', 'no element with [ref=' + id + ']') };
  return { el };
}

function isDisabled(el) {
  return !!el.disabled || el.getAttribute('aria-disabled') === 'true' || !!el.closest('fieldset[disabled]');
}

function center(el) {
  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2, rect: r };
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

function clickAt(el) {
  const { x, y } = center(el);
  const doc = el.ownerDocument;
  const hit = doc.elementFromPoint(x, y);
  // Dispatch to what is actually under the pointer when it is part of the
  // element (an inner span), the way a real click would land.
  const target = hit && (hit === el || el.contains(hit)) ? hit : el;
  const covered = hit && hit !== el && !el.contains(hit) && !hit.contains(el) ? shortDescription(hit) : null;
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
  return covered;
}

function loftClick(args) {
  const { el, error } = lookup(args.ref);
  if (error) return error;
  if (isDisabled(el)) return fail('disabled', shortDescription(el) + ' is disabled');
  const described = shortDescription(el);
  const covered = clickAt(el);
  const result = { ok: true, clicked: described };
  if (covered) result.warning = 'the element was covered by ' + covered + '; the click was dispatched to the element anyway';
  return result;
}

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

function setFieldValue(el, text) {
  // execCommand edits like real typing: trusted beforeinput/input events,
  // maxlength, undo, and framework-controlled inputs all behave.
  let done = false;
  try {
    el.focus({ preventScroll: true });
    if (typeof el.select === 'function') el.select();
    done = text === ''
      ? el.ownerDocument.execCommand('delete', false)
      : el.ownerDocument.execCommand('insertText', false, text);
  } catch (_) {
    done = false;
  }
  const max = el.maxLength > 0 ? el.maxLength : Infinity;
  const expected = text.slice(0, max);
  if (!done || el.value !== expected) {
    // Fallback: the prototype's value setter bypasses React's instance-level
    // tracker, so the input event below registers as a change.
    const setter = nativeValueSetter(el);
    if (setter) setter.call(el, expected);
    else el.value = expected;
    el.dispatchEvent(new InputEvent('input', {
      bubbles: true, composed: true, inputType: 'insertText', data: expected,
    }));
  }
  el.dispatchEvent(new Event('change', { bubbles: true }));
}

function setEditableText(el, text) {
  el.focus({ preventScroll: true });
  const doc = el.ownerDocument;
  const selection = doc.getSelection();
  const range = doc.createRange();
  range.selectNodeContents(el);
  selection.removeAllRanges();
  selection.addRange(range);
  let done = false;
  try {
    done = text === '' ? doc.execCommand('delete', false) : doc.execCommand('insertText', false, text);
  } catch (_) {
    done = false;
  }
  if (!done) {
    el.textContent = text;
    el.dispatchEvent(new InputEvent('input', { bubbles: true, composed: true, inputType: 'insertText', data: text }));
  }
}

function chooseOption(select, text) {
  const wanted = text.trim().toLowerCase();
  const options = [...select.options];
  const option =
    options.find((o) => o.value === text) ||
    options.find((o) => squash(o.label || o.text).toLowerCase() === wanted) ||
    options.find((o) => squash(o.label || o.text).toLowerCase().includes(wanted));
  if (!option) return null;
  const setter = nativeValueSetter(select);
  if (setter) setter.call(select, option.value);
  else select.value = option.value;
  select.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  select.dispatchEvent(new Event('change', { bubbles: true }));
  return squash(option.label || option.text);
}

function keyEvent(el, type, key, mods) {
  const info = keyInfo(key);
  const view = el.ownerDocument.defaultView || window;
  return el.dispatchEvent(new view.KeyboardEvent(type, {
    key, code: info.code, keyCode: info.keyCode, which: info.keyCode,
    charCode: type === 'keypress' ? info.charCode : 0,
    bubbles: true, cancelable: true, composed: true, view,
    shiftKey: !!mods.shift, ctrlKey: !!mods.ctrl, altKey: !!mods.alt, metaKey: !!mods.meta,
  }));
}

function implicitSubmit(form) {
  const button = [...form.elements].find(
    (e) => (e.type === 'submit' || e.type === 'image') && typeof e.click === 'function',
  );
  if (button) {
    if (isDisabled(button)) return 'the form\'s submit button is disabled';
    button.click();
    return 'submitted the form';
  }
  if (typeof form.requestSubmit === 'function') form.requestSubmit();
  else form.submit();
  return 'submitted the form';
}

// Presses Enter on a field to submit it: implicit form submission when the
// field is in a form and the page did not cancel the keydown.
function submitFrom(el) {
  let action;
  if (keyEvent(el, 'keydown', 'Enter', {})) {
    keyEvent(el, 'keypress', 'Enter', {});
    const form = el.form || el.closest('form');
    action = form ? implicitSubmit(form) : (enterDefault(el) || 'pressed Enter');
  } else {
    action = 'the page handled Enter';
  }
  keyEvent(el, 'keyup', 'Enter', {});
  return action;
}

function enterDefault(el) {
  if (el.localName === 'input' && el.form &&
      !['checkbox', 'radio', 'button', 'submit', 'reset', 'image', 'file'].includes(el.type)) {
    return implicitSubmit(el.form);
  }
  if (el.localName === 'a' || el.localName === 'button' || el.localName === 'summary' ||
      (el.localName === 'input' && ['submit', 'button', 'reset', 'image'].includes(el.type))) {
    el.click();
    return 'activated ' + shortDescription(el);
  }
  if (el.localName === 'textarea' || el.isContentEditable) {
    el.ownerDocument.execCommand('insertLineBreak', false);
    return 'inserted a line break';
  }
  return null;
}

function loftType(args) {
  const { el, error } = lookup(args.ref);
  if (error) return error;
  if (isDisabled(el)) return fail('disabled', shortDescription(el) + ' is disabled');
  if (el.readOnly) return fail('readonly', shortDescription(el) + ' is read-only');
  const described = shortDescription(el);
  const text = String(args.text);
  el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
  const result = { ok: true, element: described };
  if (el.localName === 'select') {
    const chosen = chooseOption(el, text);
    if (chosen === null) {
      return fail('no_option', 'no option matching "' + text + '" in ' + described +
        ' (options: ' + [...el.options].slice(0, 20).map((o) => squash(o.text)).join(', ') + ')');
    }
    result.value = chosen;
  } else if (el.localName === 'input' && ['checkbox', 'radio'].includes((el.type || '').toLowerCase())) {
    return fail('not_editable', described + ' is a ' + el.type + '; use click to toggle it');
  } else if (el.localName === 'input' || el.localName === 'textarea') {
    if (el.localName === 'input' && ['submit', 'button', 'reset', 'image', 'file'].includes(el.type)) {
      return fail('not_editable', described + ' does not accept text');
    }
    setFieldValue(el, text);
    result.value = maskedValue(el);
  } else if (el.isContentEditable) {
    setEditableText(el, text);
    result.value = clip(textOf(el), 200);
  } else {
    return fail('not_editable', described + ' is not a text field; click it first or pick an input ref');
  }
  if (args.submit) result.submit = submitFrom(el);
  return result;
}

const KEYS = {
  Enter: ['Enter', 13], Tab: ['Tab', 9], Escape: ['Escape', 27], Backspace: ['Backspace', 8],
  Delete: ['Delete', 46], ArrowDown: ['ArrowDown', 40], ArrowUp: ['ArrowUp', 38],
  ArrowLeft: ['ArrowLeft', 37], ArrowRight: ['ArrowRight', 39], Home: ['Home', 36], End: ['End', 35],
  PageUp: ['PageUp', 33], PageDown: ['PageDown', 34], ' ': ['Space', 32], Shift: ['ShiftLeft', 16],
  Control: ['ControlLeft', 17], Alt: ['AltLeft', 18], Meta: ['MetaLeft', 91],
};
const KEY_ALIASES = {
  esc: 'Escape', escape: 'Escape', return: 'Enter', enter: 'Enter', tab: 'Tab', space: ' ',
  spacebar: ' ', backspace: 'Backspace', delete: 'Delete', del: 'Delete', up: 'ArrowUp',
  down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight', arrowup: 'ArrowUp',
  arrowdown: 'ArrowDown', arrowleft: 'ArrowLeft', arrowright: 'ArrowRight', home: 'Home',
  end: 'End', pageup: 'PageUp', pagedown: 'PageDown', pgup: 'PageUp', pgdn: 'PageDown',
};
const MODIFIERS = {
  shift: 'shift', ctrl: 'ctrl', control: 'ctrl', alt: 'alt', option: 'alt', opt: 'alt',
  meta: 'meta', cmd: 'meta', command: 'meta', super: 'meta',
};

function keyInfo(key) {
  if (KEYS[key]) return { code: KEYS[key][0], keyCode: KEYS[key][1], charCode: key === 'Enter' ? 13 : 0 };
  if (/^F\d{1,2}$/.test(key)) return { code: key, keyCode: 111 + Number(key.slice(1)), charCode: 0 };
  if (key.length === 1) {
    const upper = key.toUpperCase();
    let code = '';
    if (/[A-Z]/.test(upper)) code = 'Key' + upper;
    else if (/[0-9]/.test(key)) code = 'Digit' + key;
    return { code, keyCode: upper.charCodeAt(0), charCode: key.charCodeAt(0) };
  }
  return { code: key, keyCode: 0, charCode: 0 };
}

function parseKey(spec) {
  const mods = {};
  if (spec.length === 1) return { key: spec, mods };
  const parts = spec.split('+');
  let last = parts.pop();
  if (last === '' && parts.length) {
    // "Shift++" → the "+" key.
    parts.pop();
    last = '+';
  }
  for (const p of parts) {
    const m = MODIFIERS[p.trim().toLowerCase()];
    if (m) mods[m] = true;
  }
  last = last.trim();
  const key = KEY_ALIASES[last.toLowerCase()] ||
    (last.length === 1 ? last : last[0].toUpperCase() + last.slice(1));
  return { key: mods.shift && key.length === 1 ? key.toUpperCase() : key, mods };
}

function tabbables(doc) {
  const all = [...doc.querySelectorAll(
    'a[href],area[href],button,input:not([type=hidden]),select,textarea,iframe,summary,[tabindex],[contenteditable=""],[contenteditable=true]',
  )].filter((e) => e.tabIndex >= 0 && !e.disabled && e.getClientRects().length &&
    styleOf(e).visibility !== 'hidden');
  const positive = all.filter((e) => e.tabIndex > 0).sort((a, b) => a.tabIndex - b.tabIndex);
  return positive.concat(all.filter((e) => e.tabIndex === 0));
}

function moveFocus(from, backwards) {
  const list = tabbables(from.ownerDocument || document);
  if (!list.length) return null;
  const index = list.indexOf(from);
  const next = index < 0
    ? list[backwards ? list.length - 1 : 0]
    : list[(index + (backwards ? -1 : 1) + list.length) % list.length];
  next.focus();
  if (isTextField(next) && typeof next.select === 'function') next.select();
  return 'moved focus to ' + shortDescription(next);
}

function scrollPage(dy) {
  window.scrollBy({ top: dy, left: 0, behavior: 'instant' });
  return 'scrolled the page';
}

function keyDefault(el, key, mods) {
  const editable = isTextField(el) || el.isContentEditable;
  const doc = el.ownerDocument;
  if (mods.meta || mods.ctrl) {
    if (key.toLowerCase() === 'a') {
      if (editable && typeof el.select === 'function') el.select();
      else doc.execCommand('selectAll', false);
      return 'selected all';
    }
    return null;
  }
  switch (key) {
    case 'Enter':
      return enterDefault(el);
    case 'Tab':
      return moveFocus(el, !!mods.shift);
    case ' ':
      if (editable) break;
      if (['button', 'summary'].includes(el.localName) ||
          (el.localName === 'input' && ['checkbox', 'radio', 'button', 'submit'].includes(el.type))) {
        el.click();
        return 'activated ' + shortDescription(el);
      }
      return scrollPage((mods.shift ? -0.85 : 0.85) * window.innerHeight);
    case 'ArrowDown':
    case 'ArrowUp':
      if (el.localName === 'select') {
        const step = key === 'ArrowDown' ? 1 : -1;
        const index = Math.max(0, Math.min(el.options.length - 1, el.selectedIndex + step));
        if (index !== el.selectedIndex) {
          el.selectedIndex = index;
          el.dispatchEvent(new Event('input', { bubbles: true }));
          el.dispatchEvent(new Event('change', { bubbles: true }));
        }
        return 'selected ' + squash(el.options[index] ? el.options[index].text : '');
      }
      if (editable) return null;
      return scrollPage(key === 'ArrowDown' ? 40 : -40);
    case 'PageDown':
    case 'PageUp':
      if (editable && el.localName === 'textarea') return null;
      return scrollPage((key === 'PageDown' ? 0.85 : -0.85) * window.innerHeight);
    case 'Home':
    case 'End':
      if (editable) return null;
      window.scrollTo({ top: key === 'Home' ? 0 : document.documentElement.scrollHeight, behavior: 'instant' });
      return 'scrolled to the ' + (key === 'Home' ? 'top' : 'bottom');
    case 'Backspace':
    case 'Delete':
      if (!editable) return null;
      doc.execCommand(key === 'Backspace' ? 'delete' : 'forwardDelete', false);
      return 'deleted';
    case 'Escape':
      return null;
  }
  if (key.length === 1 && editable) {
    if (!doc.execCommand('insertText', false, key)) return null;
    return 'inserted "' + key + '"';
  }
  return null;
}

function loftPress(args) {
  const { key, mods } = parseKey(String(args.key || ''));
  if (!key) return fail('bad_key', 'empty key');
  const el = deepActiveElement();
  const target = shortDescription(el);
  let action = null;
  if (keyEvent(el, 'keydown', key, mods)) {
    if (key.length === 1 && !mods.ctrl && !mods.meta) keyEvent(el, 'keypress', key, mods);
    if (key === 'Enter') keyEvent(el, 'keypress', key, mods);
    action = keyDefault(el, key, mods);
  } else {
    action = 'the page handled the key';
  }
  keyEvent(el, 'keyup', key, mods);
  const result = { ok: true, key, target };
  if (action) result.default_action = action;
  return result;
}

function loftScroll(args) {
  const dy = Number(args.dy) || 0;
  const scroller = document.scrollingElement || document.documentElement;
  const before = scroller.scrollTop;
  window.scrollBy({ top: dy, left: 0, behavior: 'instant' });
  let target = 'page';
  let box = scroller;
  if (scroller.scrollTop === before && dy !== 0) {
    // The document does not scroll (app shells): scroll the scrollable
    // container under the viewport center instead.
    let el = document.elementFromPoint(window.innerWidth / 2, window.innerHeight / 2);
    while (el && el !== document.body && el !== document.documentElement) {
      const s = styleOf(el);
      if (/(auto|scroll|overlay)/.test(s.overflowY) && el.scrollHeight > el.clientHeight + 1) {
        const prev = el.scrollTop;
        el.scrollBy({ top: dy, left: 0, behavior: 'instant' });
        if (el.scrollTop !== prev) {
          target = shortDescription(el);
          box = el;
          break;
        }
      }
      el = el.parentElement;
    }
  }
  const viewportHeight = box === scroller ? window.innerHeight : box.clientHeight;
  const top = Math.round(box.scrollTop);
  return {
    ok: true,
    target,
    scroll_y: top,
    scroll_height: box.scrollHeight,
    viewport_height: viewportHeight,
    at_top: top <= 0,
    at_bottom: top + viewportHeight >= box.scrollHeight - 1,
  };
}
