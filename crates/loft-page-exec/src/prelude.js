// What a `browser_execute` script sees (ADR-0017 §10): `page`, locators,
// `browse`, `snapshot`, `ref`, `handoff`, `console`, timers and `state`, in
// the style of Playwright's public API, every step one browser command.
//
// Evaluated once per run as `loft.js`; the engine (`engine.rs`) calls the
// function below with its natives and gets back the functions it drives the
// run with. Nothing here is reachable from the script but what it is given.
//
// N (natives, all synchronous):
//   call(name, argsJson) → id, or an error message (string)
//   sleep(ms) → id          cancel(id)
//   info(tab) → '{"url","title"}' or null
//   picture(index) → base64 or null
//   log(level, text)        warn(text)
// The engine settles ids by calling `settle(id, ok, payload)`.
(function (N, MAIN_TAB, STATE_JSON) {
  'use strict';

  const IMAGE_KEY = '__loft_image';
  const LOCATOR_TIMEOUT = 10000;
  const WAIT_TIMEOUT = 30000;
  const POLL = 100;

  const hide = (object, props) => {
    for (const key of Object.keys(props)) {
      Object.defineProperty(object, key, { value: props[key], writable: true, configurable: true, enumerable: false });
    }
    return object;
  };
  const warn = (text) => N.warn(String(text));
  const bigints = (_, v) => (typeof v === 'bigint' ? v.toString() : v);
  const safeString = (v) => {
    try {
      return String(v);
    } catch (_) {
      return Object.prototype.toString.call(v);
    }
  };

  // ---------------------------------------------------------------- errors

  // Where in the script a call was made, for errors that surface after an
  // await (when the script's line is no longer on the stack).
  const sites = new WeakMap();
  const inScript = (stack) => typeof stack === 'string' && stack.includes('script.js:');
  function located(error, site) {
    if (error instanceof Error && !inScript(error.stack) && !sites.has(error) && inScript(site.stack)) {
      sites.set(error, site.stack);
    }
    return error;
  }
  function timeoutError(message) {
    const error = new Error(message);
    error.name = 'TimeoutError';
    return error;
  }
  function unsupported(call, advice) {
    return new Error(`${call} is not available in an Agentloft script: ${advice}`);
  }

  // Every public method: errors carry the script line that called it.
  function guard(proto) {
    for (const key of Object.getOwnPropertyNames(proto)) {
      if (key === 'constructor' || key === 'toString' || key === 'toJSON' || key.startsWith('_')) continue;
      const descriptor = Object.getOwnPropertyDescriptor(proto, key);
      if (typeof descriptor.value !== 'function') continue;
      const method = descriptor.value;
      const wrapped = {
        [key](...args) {
          const site = new Error();
          let out;
          try {
            out = method.apply(this, args);
          } catch (error) {
            throw located(error, site);
          }
          if (out instanceof Promise) {
            return out.catch((error) => {
              throw located(error, site);
            });
          }
          return out;
        },
      }[key];
      Object.defineProperty(proto, key, { value: wrapped, writable: true, configurable: true, enumerable: false });
    }
  }

  function formatItem(value) {
    if (typeof value === 'string') return value;
    if (value === undefined) return 'undefined';
    if (typeof value === 'function') return `[Function: ${value.name || 'anonymous'}]`;
    if (value instanceof Error) return `${value.name}: ${value.message}`;
    try {
      const json = JSON.stringify(value, bigints);
      if (json !== undefined) return json;
    } catch (_) {
      // Falls back to String below.
    }
    return safeString(value);
  }

  function formatError(error) {
    if (error === null) {
      return 'Error: the script most likely ran out of memory (a script has 64 MB; keep large data in the page and return only what you need)';
    }
    if (error instanceof Error) {
      const name = safeString(error.name || 'Error');
      const message = safeString(error.message ?? '');
      const head = message ? `${name}: ${message}` : name;
      const stack = inScript(error.stack) ? error.stack : sites.get(error) || '';
      // `script.js:<line>`: columns on line 1 count the wrapper's header.
      const frames = [];
      let last = null;
      let repeats = 0;
      const flush = () => {
        if (repeats > 0) frames.push(`    … the same ${repeats} more time${repeats === 1 ? '' : 's'}`);
        repeats = 0;
      };
      for (const raw of String(stack).split('\n')) {
        if (!raw.includes('script.js:')) continue;
        const frame = '    ' + raw.trim().replace(/(script\.js:\d+):\d+/, '$1');
        if (frame === last) {
          repeats++;
          continue;
        }
        flush();
        frames.push(frame);
        last = frame;
      }
      flush();
      return [head, ...frames].join('\n');
    }
    return 'Thrown: ' + formatItem(error);
  }

  // ------------------------------------------------------- host requests

  const pending = new Map();

  // One browser command; `args` may name the `tab`.
  function send(name, args) {
    const site = new Error();
    let json;
    try {
      json = JSON.stringify(args === undefined ? null : args, bigints);
    } catch (error) {
      return Promise.reject(located(error, site));
    }
    if (json === undefined) json = JSON.stringify(safeString(args));
    const id = N.call(String(name), json);
    if (typeof id === 'string') {
      site.message = id;
      return Promise.reject(site);
    }
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject, site }));
  }

  function sleep(ms) {
    const id = N.sleep(Math.max(0, Number(ms) || 0));
    const promise = new Promise((resolve) => pending.set(id, { resolve, reject: resolve, site: null }));
    return { id, promise };
  }
  const delay = (ms) => sleep(ms).promise;

  function settle(id, ok, payload) {
    const waiting = pending.get(id);
    if (!waiting) return;
    pending.delete(id);
    if (!ok) {
      const error = waiting.site || new Error();
      error.message = payload;
      if (/timeout|timed out/i.test(payload)) error.name = 'TimeoutError';
      waiting.reject(error);
      return;
    }
    let value;
    try {
      value = decode(JSON.parse(payload));
    } catch (error) {
      waiting.reject(error);
      return;
    }
    waiting.resolve(value);
  }

  // ---------------------------------------------------------------- pictures

  class Picture {
    constructor(meta) {
      hide(this, { _index: meta[IMAGE_KEY] });
      this.type = 'image';
      this.mimeType = meta.mime_type;
      this.width = meta.width;
      this.height = meta.height;
      this.size = meta.size;
      for (const key of Object.keys(meta)) {
        if (['mime_type', 'width', 'height', 'size', 'data', IMAGE_KEY].includes(key) || key in this) continue;
        this[key] = meta[key];
      }
    }
    get length() {
      return this.size;
    }
    get byteLength() {
      return this.size;
    }
    toJSON() {
      return { type: 'image', mimeType: this.mimeType, width: this.width, height: this.height, size: this.size };
    }
    toString(encoding) {
      if (encoding === 'base64') {
        const data = N.picture(this._index);
        if (data == null) throw new Error('the picture is no longer available');
        return data;
      }
      return `[${this.mimeType} picture, ${this.width}×${this.height}, ${this.size} bytes]`;
    }
  }

  function decode(value) {
    if (value && typeof value === 'object' && !Array.isArray(value) && typeof value[IMAGE_KEY] === 'number') {
      return new Picture(value);
    }
    return value;
  }

  // -------------------------------------------------------------- console

  function line(level, items) {
    N.log(level, items.map(formatItem).join(' '));
  }
  const console = {
    log: (...items) => line('log', items),
    info: (...items) => line('info', items),
    warn: (...items) => line('warn', items),
    error: (...items) => line('error', items),
    debug: (...items) => line('debug', items),
    trace: (...items) => line('debug', items),
    dir: (...items) => line('log', items),
    table: (...items) => line('log', items),
    assert: (condition, ...items) => {
      if (!condition) line('error', items.length ? ['Assertion failed:', ...items] : ['Assertion failed']);
    },
    time: () => {},
    timeEnd: () => {},
    timeLog: () => {},
    group: () => {},
    groupCollapsed: () => {},
    groupEnd: () => {},
    count: () => {},
    countReset: () => {},
  };

  // --------------------------------------------------------------- timers

  let lastTimer = 0;
  const timers = new Map();

  function timerFailed(error) {
    line('error', ['Uncaught error in a timer:', error]);
  }
  function schedule(handle, timer) {
    const { id, promise } = sleep(timer.ms);
    timer.id = id;
    promise.then(() => {
      if (timers.get(handle) !== timer) return;
      if (!timer.repeat) timers.delete(handle);
      try {
        const out = timer.fn(...timer.args);
        if (out && typeof out.then === 'function') out.then(undefined, timerFailed);
      } catch (error) {
        timerFailed(error);
      }
      if (timer.repeat && timers.get(handle) === timer) schedule(handle, timer);
    });
  }
  function addTimer(kind, fn, ms, args, repeat) {
    if (typeof fn !== 'function') throw new TypeError(`${kind} needs a function to call`);
    let wait = Math.max(0, Number(ms) || 0);
    if (repeat) wait = Math.max(10, wait);
    const handle = ++lastTimer;
    const timer = { fn, args, ms: wait, repeat, id: 0 };
    timers.set(handle, timer);
    schedule(handle, timer);
    return handle;
  }
  function clearTimer(handle) {
    const timer = timers.get(handle);
    if (!timer) return;
    timers.delete(handle);
    N.cancel(timer.id);
    pending.delete(timer.id);
  }
  const setTimeout = (fn, ms, ...args) => addTimer('setTimeout', fn, ms, args, false);
  const setInterval = (fn, ms, ...args) => addTimer('setInterval', fn, ms, args, true);
  const clearTimeout = (handle) => clearTimer(handle);
  const clearInterval = (handle) => clearTimer(handle);
  function queueMicrotask(fn) {
    if (typeof fn !== 'function') throw new TypeError('queueMicrotask needs a function');
    Promise.resolve().then(() => fn());
  }
  const structuredClone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value, bigints)));

  function fetch() {
    throw new Error(
      "fetch is not available in a script: fetching belongs to the page. Fetch inside the page instead: " +
        "await page.evaluate(async (url) => (await fetch(url)).text(), 'https://…')",
    );
  }
  function require(name) {
    throw new Error(
      `require(${JSON.stringify(safeString(name))}): a script has no Node modules; use page, browse.* and snapshot()`,
    );
  }
  const files = new Proxy(
    {},
    {
      get(_, key) {
        if (typeof key !== 'string' || key === 'then' || key === 'toJSON') return undefined;
        return () => {
          throw new Error(
            `files.${key}(): a script has no files. It runs inside Agentloft, outside your own sandbox and ` +
              'permissions, so it can neither read nor write any: return the data from the script and write it with your own tools',
          );
        };
      },
    },
  );

  // ---------------------------------------------------------------- state

  let state = {};
  try {
    const parsed = JSON.parse(STATE_JSON);
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) state = parsed;
  } catch (_) {
    // Not JSON: a fresh state.
  }

  // ------------------------------------------------------------ selectors

  function textStep(key, value, exact) {
    if (value instanceof RegExp) return { [key]: value.source, regex: value.flags || true };
    return { [key]: safeString(value), exact: !!exact };
  }

  function unescape(text) {
    return text.replace(/\\(.)/g, '$1');
  }

  // `text=` and quoted parts: /re/flags, "exact", "loose"i, or loose.
  function textSelector(key, raw) {
    const text = raw.trim();
    let m = /^\/(.*)\/([a-z]*)$/s.exec(text);
    if (m) return { [key]: m[1], regex: m[2] || true };
    m = /^(["'])(.*)\1([si]?)$/s.exec(text);
    if (m) return { [key]: unescape(m[2]), exact: m[3] !== 'i' };
    return { [key]: text, exact: false };
  }

  function unquote(text) {
    const m = /^(["'])(.*)\1$/s.exec(text.trim());
    return m ? unescape(m[2]) : text.trim();
  }

  // role=button[name="Save"i][level=2]
  function roleSelector(rest) {
    const m = /^([a-z]+)(.*)$/s.exec(rest.trim());
    if (!m) throw new Error(`role=${rest}: a role selector needs a role name, e.g. role=button[name="Save"]`);
    const step = { role: m[1] };
    const attribute = /\[\s*([a-z-]+)\s*(?:=\s*("(?:[^"\\]|\\.)*"[si]?|'(?:[^'\\]|\\.)*'[si]?|[^\]]*?))?\s*\]/g;
    let a;
    while ((a = attribute.exec(m[2]))) {
      const key = a[1];
      const value = a[2] === undefined ? 'true' : a[2].trim();
      if (key === 'name') Object.assign(step, textSelector('name', value));
      else if (key === 'level') step.level = Number(unquote(value));
      else if (key === 'checked') step.checked = unquote(value) === 'true';
      else if (key === 'include-hidden') step.include_hidden = unquote(value) !== 'false';
    }
    return step;
  }

  // Split on `>>` outside quotes.
  function splitSelector(selector) {
    const parts = [];
    let current = '';
    let quote = null;
    for (let i = 0; i < selector.length; i++) {
      const c = selector[i];
      if (quote) {
        current += c;
        if (c === '\\' && i + 1 < selector.length) current += selector[++i];
        else if (c === quote) quote = null;
      } else if (c === '"' || c === "'") {
        quote = c;
        current += c;
      } else if (c === '>' && selector[i + 1] === '>') {
        parts.push(current);
        current = '';
        i++;
      } else {
        current += c;
      }
    }
    parts.push(current);
    return parts.map((part) => part.trim()).filter(Boolean);
  }

  function partStep(part) {
    let m;
    if ((m = /^nth=(-?\d+)$/.exec(part))) return { nth: Number(m[1]) };
    if ((m = /^visible=(true|false)$/.exec(part))) return { visible: m[1] === 'true' };
    if (part.startsWith('text=')) return textSelector('text', part.slice(5));
    if (part.startsWith('role=')) return roleSelector(part.slice(5));
    if ((m = /^data-test-?id=(.*)$/s.exec(part))) return { testid: unquote(m[1]), exact: true };
    if (part.startsWith('id=')) return { css: `[id=${JSON.stringify(unquote(part.slice(3)))}]` };
    if (part.startsWith('css=')) return { css: part.slice(4) };
    if (part.startsWith('xpath=') || part.startsWith('//') || part.startsWith('..')) {
      throw unsupported(`the XPath selector ${JSON.stringify(part)}`, "use CSS, text=…, role=… or page.getByRole(…)");
    }
    if (/^(["']).*\1$/s.test(part)) return textSelector('text', part);
    return { css: part };
  }

  function parseSelector(selector) {
    if (typeof selector !== 'string') throw new TypeError(`a selector must be a string or a locator, not ${formatItem(selector)}`);
    const steps = splitSelector(selector).map(partStep);
    if (!steps.length) throw new Error(`the selector ${JSON.stringify(selector)} is empty`);
    return steps;
  }

  function selectorSteps(selector) {
    if (selector instanceof Locator) {
      if (selector._ref !== undefined) {
        throw unsupported(`chaining ${selector}`, "a ref locator can't be combined; start from a selector, e.g. page.getByRole('button', {name: …})");
      }
      return selector._steps.slice();
    }
    return parseSelector(selector);
  }

  function filterSteps(options) {
    const steps = [];
    if (!options || typeof options !== 'object') return steps;
    if (options.has !== undefined || options.hasNot !== undefined) {
      throw unsupported('filter({has}) and filter({hasNot})', 'chain the inner locator instead (outer.locator(inner)), or use filter({hasText})');
    }
    if (options.hasText !== undefined) steps.push(textStep('has_text', options.hasText, false));
    if (options.hasNotText !== undefined) steps.push(textStep('has_not_text', options.hasNotText, false));
    if (options.visible !== undefined) steps.push({ visible: !!options.visible });
    return steps;
  }

  function roleStep(role, options = {}) {
    const step = { role: safeString(role) };
    options = options || {};
    if (options.name !== undefined) Object.assign(step, textStep('name', options.name, options.exact));
    if (options.level !== undefined) step.level = Number(options.level);
    if (options.checked !== undefined) step.checked = !!options.checked;
    if (options.includeHidden) step.include_hidden = true;
    return step;
  }

  const lastIsNth = (steps) => steps.length > 0 && 'nth' in steps[steps.length - 1];

  const timeoutOr = (value, fallback) => {
    const n = Number(value);
    return value === undefined || value === null || !Number.isFinite(n) ? fallback : Math.max(0, n);
  };

  // ------------------------------------------- functions run on elements

  const ON = {
    attached: '() => 1',
    textContent: '(el) => el.textContent',
    innerText: '(el) => el.innerText',
    innerHTML: '(el) => el.innerHTML',
    inputValue:
      "(el) => { if (!['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName)) throw new Error('inputValue: the element is not an <input>, <textarea> or <select>'); return el.value; }",
    getAttribute: '(el, name) => el.getAttribute(name)',
    isChecked:
      "(el) => (typeof el.type === 'string' && 'checked' in el) ? !!el.checked : el.getAttribute('aria-checked') === 'true'",
    isDisabled: "(el) => !!el.disabled || el.getAttribute('aria-disabled') === 'true'",
    isEditable:
      "(el) => !(el.disabled || el.getAttribute('aria-disabled') === 'true') && !el.readOnly && (el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName))",
    focus: '(el) => { el.focus(); return null; }',
    blur: '(el) => { el.blur(); return null; }',
    dispatchEvent:
      '(el, a) => { const init = Object.assign({ bubbles: true, cancelable: true }, a.init || {}); const event = new Event(a.type, init); for (const k of Object.keys(init)) { if (!(k in event)) { try { event[k] = init[k]; } catch (e) {} } } el.dispatchEvent(event); return null; }',
    boundingBox:
      '(el) => { const r = el.getBoundingClientRect(); return r.width === 0 && r.height === 0 ? null : { x: r.x, y: r.y, width: r.width, height: r.height }; }',
    scrollIntoView: "(el) => { el.scrollIntoView({ block: 'center', inline: 'center' }); return null; }",
    visible:
      "(el) => { const r = el.getBoundingClientRect(); const s = getComputedStyle(el); return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; }",
    allTextContents: "(els) => els.map((el) => el.textContent ?? '')",
    allInnerTexts: "(els) => els.map((el) => el.innerText ?? '')",
    selectOptions:
      "(el, items) => { if (!(el instanceof HTMLSelectElement)) throw new Error('selectOption: the element is not a <select>'); const options = Array.from(el.options); for (const o of options) o.selected = false; const chosen = []; for (const item of items) { let o; if (item.index !== undefined) o = options[item.index]; if (!o && item.value !== undefined) o = options.find((x) => x.value === item.value); if (!o && item.label !== undefined) o = options.find((x) => x.label === item.label); if (!o && item.value !== undefined) o = options.find((x) => x.label === item.value); if (!o) throw new Error('selectOption: no option matches ' + JSON.stringify(item)); o.selected = true; chosen.push(o.value); } el.dispatchEvent(new Event('input', { bubbles: true })); el.dispatchEvent(new Event('change', { bubbles: true })); return chosen; }",
  };

  // -------------------------------------------------------------- locators

  class Locator {
    constructor(page, steps, ref) {
      hide(this, { _page: page, _steps: steps, _ref: ref });
    }

    toString() {
      return this._ref !== undefined ? `ref(${this._ref})` : JSON.stringify(this._steps);
    }

    _chain(steps, what) {
      if (this._ref !== undefined) {
        throw unsupported(`${this}.${what}`, "a ref locator can't be narrowed; start from a selector, e.g. page.getByRole('button', {name: …})");
      }
      return new Locator(this._page, this._steps.concat(steps));
    }

    _send(name, args) {
      return this._page._send(name, args);
    }

    // The one element an action needs, as a ref: strict, and waiting for
    // it to appear.
    async _resolve(timeout) {
      if (this._ref !== undefined) return this._ref;
      const limit = timeoutOr(timeout, this._page._timeout ?? LOCATOR_TIMEOUT);
      const start = Date.now();
      for (;;) {
        const found = await this._send('locate', { steps: this._steps, limit: 5 });
        const count = Number(found && found.count) || 0;
        const refs = (found && found.refs) || [];
        if (count === 1 || (count > 1 && lastIsNth(this._steps))) return refs[0];
        if (count > 1) {
          const described = (found.elements || []).map(safeString);
          const more = count > described.length ? ', …' : '';
          throw new Error(
            `${this}: matched ${count} elements (${described.join(', ')}${more}), but an action needs one; ` +
              'use .first(), .nth(i) or a narrower locator',
          );
        }
        if (Date.now() - start >= limit) {
          throw timeoutError(`${this}: the locator found nothing within ${limit} ms`);
        }
        await delay(POLL);
      }
    }

    async _on(fn, arg, timeout) {
      const ref = await this._resolve(timeout);
      return this._send('eval_on', { refs: [ref], function: fn, arg });
    }

    async _all(fn, arg) {
      let refs;
      if (this._ref !== undefined) {
        refs = [this._ref];
      } else {
        const found = await this._send('locate', { steps: this._steps, limit: 500 });
        refs = Array.isArray(found && found.refs) ? found.refs : [];
        const count = Number(found && found.count) || 0;
        if (count > refs.length) warn(`${this}: only the first ${refs.length} of ${count} elements were read`);
      }
      return this._send('eval_on', { refs, function: fn, arg, all: true });
    }

    // Composition
    page() {
      return this._page;
    }
    locator(selector, options) {
      return this._chain(selectorSteps(selector).concat(filterSteps(options)), 'locator()');
    }
    getByRole(role, options) {
      return this._chain([roleStep(role, options)], 'getByRole()');
    }
    getByText(text, options = {}) {
      return this._chain([textStep('text', text, options && options.exact)], 'getByText()');
    }
    getByLabel(text, options = {}) {
      return this._chain([textStep('label', text, options && options.exact)], 'getByLabel()');
    }
    getByPlaceholder(text, options = {}) {
      return this._chain([textStep('placeholder', text, options && options.exact)], 'getByPlaceholder()');
    }
    getByAltText(text, options = {}) {
      return this._chain([textStep('alt', text, options && options.exact)], 'getByAltText()');
    }
    getByTitle(text, options = {}) {
      return this._chain([textStep('title', text, options && options.exact)], 'getByTitle()');
    }
    getByTestId(id) {
      return this._chain([textStep('testid', id, true)], 'getByTestId()');
    }
    filter(options) {
      const steps = filterSteps(options);
      return steps.length ? this._chain(steps, 'filter()') : this;
    }
    first() {
      return this._ref !== undefined ? this : this._chain([{ nth: 0 }], 'first()');
    }
    last() {
      return this._ref !== undefined ? this : this._chain([{ nth: -1 }], 'last()');
    }
    nth(index) {
      if (this._ref !== undefined && Number(index) === 0) return this;
      return this._chain([{ nth: Math.trunc(Number(index)) || 0 }], 'nth()');
    }
    and() {
      throw unsupported('locator.and()', 'narrow with filter({hasText}) or a CSS selector');
    }
    or() {
      throw unsupported('locator.or()', 'try each locator in turn with count()');
    }
    frameLocator() {
      throw unsupported('locator.frameLocator()', "locators already look inside same-origin frames; another site's frames can't be reached");
    }
    contentFrame() {
      throw unsupported('locator.contentFrame()', "locators already look inside same-origin frames; another site's frames can't be reached");
    }

    // Actions
    async _click(options, count) {
      options = options || {};
      if (options.trial) {
        await this.waitFor({ timeout: options.timeout });
        return undefined;
      }
      if (options.position !== undefined) warn('click({position}) is ignored: the centre of the element is clicked');
      const ref = await this._resolve(options.timeout);
      return this._send('click', { ref_id: ref, button: options.button, count, modifiers: options.modifiers });
    }
    click(options = {}) {
      return this._click(options, options && options.clickCount);
    }
    dblclick(options = {}) {
      return this._click(options, 2);
    }
    tap(options = {}) {
      return this._click(options, options && options.clickCount);
    }
    async fill(value, options = {}) {
      const ref = await this._resolve(options && options.timeout);
      return this._send('fill', { ref_id: ref, value: safeString(value) });
    }
    clear(options = {}) {
      return this.fill('', options);
    }
    async type(text, options = {}) {
      const ref = await this._resolve(options && options.timeout);
      return this._send('type', { ref_id: ref, text: safeString(text) });
    }
    pressSequentially(text, options = {}) {
      return this.type(text, options);
    }
    async press(key, options = {}) {
      const ref = await this._resolve(options && options.timeout);
      return this._send('press', { ref_id: ref, key: safeString(key) });
    }
    async hover(options = {}) {
      const ref = await this._resolve(options && options.timeout);
      return this._send('hover', { ref_id: ref });
    }
    async setChecked(checked, options = {}) {
      checked = !!checked;
      if ((await this.isChecked(options)) === checked) return undefined;
      await this.click(options);
      if ((await this.isChecked(options)) !== checked) {
        throw new Error(`${this}: clicking it did not make it ${checked ? 'checked' : 'unchecked'}`);
      }
      return undefined;
    }
    check(options = {}) {
      return this.setChecked(true, options);
    }
    uncheck(options = {}) {
      return this.setChecked(false, options);
    }
    async selectOption(values, options = {}) {
      const list = values == null ? [] : Array.isArray(values) ? values : [values];
      const items = list.map((v) => {
        if (typeof v === 'string' || typeof v === 'number') return { value: String(v) };
        if (v && typeof v === 'object' && ('value' in v || 'label' in v || 'index' in v)) {
          return {
            value: v.value === undefined ? undefined : String(v.value),
            label: v.label === undefined ? undefined : String(v.label),
            index: v.index === undefined ? undefined : Number(v.index),
          };
        }
        throw new TypeError(`selectOption: ${formatItem(v)} is not an option value, {value}, {label} or {index}`);
      });
      const ref = await this._resolve(options && options.timeout);
      if (items.length && !items.some((item) => item.index !== undefined)) {
        const result = await this._send('select', { ref_id: ref, values: items.map((item) => item.value ?? item.label) });
        if (result && result.values !== undefined) return result.values;
        if (result && result.selected !== undefined) return result.selected;
        return result;
      }
      return this._send('eval_on', { refs: [ref], function: ON.selectOptions, arg: items });
    }
    async setInputFiles(files, options = {}) {
      const list = Array.isArray(files) ? files : [files];
      const ref = await this._resolve(options && options.timeout);
      return this._send('upload', { ref_id: ref, files: list });
    }
    async dragTo(target, options = {}) {
      if (!(target instanceof Locator)) throw new TypeError('dragTo needs a locator to drop on');
      const from = await this._resolve(options && options.timeout);
      const to = await target._resolve(options && options.timeout);
      return this._send('drag', { from: { ref_id: from }, to: { ref_id: to } });
    }

    // Reads in the page
    evaluate(fn, arg, options = {}) {
      let source;
      if (typeof fn === 'function') source = fn.toString();
      else if (typeof fn === 'string') source = `(el, arg) => (${fn})`;
      else throw new TypeError('evaluate needs a function (el, arg) => …, or an expression of el');
      return this._on(source, arg, options && options.timeout);
    }
    evaluateAll(fn, arg) {
      if (typeof fn !== 'function') throw new TypeError('evaluateAll needs a function (elements, arg) => …');
      return this._all(fn.toString(), arg);
    }
    evaluateHandle() {
      throw unsupported('evaluateHandle()', 'use evaluate() and return plain data');
    }
    textContent(options = {}) {
      return this._on(ON.textContent, undefined, options && options.timeout);
    }
    innerText(options = {}) {
      return this._on(ON.innerText, undefined, options && options.timeout);
    }
    innerHTML(options = {}) {
      return this._on(ON.innerHTML, undefined, options && options.timeout);
    }
    inputValue(options = {}) {
      return this._on(ON.inputValue, undefined, options && options.timeout);
    }
    getAttribute(name, options = {}) {
      return this._on(ON.getAttribute, safeString(name), options && options.timeout);
    }
    isChecked(options = {}) {
      return this._on(ON.isChecked, undefined, options && options.timeout);
    }
    isDisabled(options = {}) {
      return this._on(ON.isDisabled, undefined, options && options.timeout);
    }
    async isEnabled(options = {}) {
      return !(await this._on(ON.isDisabled, undefined, options && options.timeout));
    }
    isEditable(options = {}) {
      return this._on(ON.isEditable, undefined, options && options.timeout);
    }
    focus(options = {}) {
      return this._on(ON.focus, undefined, options && options.timeout);
    }
    blur(options = {}) {
      return this._on(ON.blur, undefined, options && options.timeout);
    }
    dispatchEvent(type, init, options = {}) {
      return this._on(ON.dispatchEvent, { type: safeString(type), init: init || {} }, options && options.timeout);
    }
    boundingBox(options = {}) {
      return this._on(ON.boundingBox, undefined, options && options.timeout);
    }
    scrollIntoViewIfNeeded(options = {}) {
      return this._on(ON.scrollIntoView, undefined, options && options.timeout);
    }
    async highlight() {
      return undefined;
    }
    async screenshot(options = {}) {
      options = options || {};
      if (options.path !== undefined) warn('screenshot({path}): no file is written; return the picture from the script to see it');
      for (const key of ['mask', 'animations', 'omitBackground']) {
        if (options[key] !== undefined) warn(`screenshot({${key}}) is ignored`);
      }
      const ref = await this._resolve(options.timeout);
      return this._send('screenshot', { ref_id: ref, format: options.type, quality: options.quality, scale: options.scale });
    }

    // Queries that don't wait
    async isVisible() {
      try {
        let ref = this._ref;
        if (ref === undefined) {
          const found = await this._send('locate', { steps: this._steps, limit: 1 });
          const count = Number(found && found.count) || 0;
          if (!count) return false;
          if (count > 1 && !lastIsNth(this._steps)) return true;
          ref = found.refs[0];
        }
        return !!(await this._send('eval_on', { refs: [ref], function: ON.visible }));
      } catch (_) {
        return false;
      }
    }
    async isHidden() {
      return !(await this.isVisible());
    }
    async count() {
      if (this._ref !== undefined) {
        try {
          await this._send('eval_on', { refs: [this._ref], function: ON.attached });
          return 1;
        } catch (_) {
          return 0;
        }
      }
      const found = await this._send('locate', { steps: this._steps, limit: 0 });
      return Number(found && found.count) || 0;
    }
    async all() {
      if (this._ref !== undefined) return [this];
      const count = await this.count();
      return Array.from({ length: count }, (_, i) => this.nth(i));
    }
    elementHandles() {
      return this.all();
    }
    async elementHandle() {
      return this;
    }
    allTextContents() {
      return this._all(ON.allTextContents);
    }
    allInnerTexts() {
      return this._all(ON.allInnerTexts);
    }

    async waitFor(options = {}) {
      options = options || {};
      const want = options.state === undefined ? 'visible' : options.state;
      if (!['attached', 'detached', 'visible', 'hidden'].includes(want)) {
        throw new Error(`waitFor: state must be "attached", "detached", "visible" or "hidden", not ${formatItem(want)}`);
      }
      const limit = timeoutOr(options.timeout, this._page._timeout ?? LOCATOR_TIMEOUT);
      const start = Date.now();
      for (;;) {
        let done;
        if (want === 'attached') done = (await this.count()) > 0;
        else if (want === 'detached') done = (await this.count()) === 0;
        else if (want === 'visible') done = await this.isVisible();
        else done = !(await this.isVisible());
        if (done) return undefined;
        if (Date.now() - start >= limit) {
          throw timeoutError(`${this}: did not become ${want} within ${limit} ms`);
        }
        await delay(POLL);
      }
    }
  }

  // ----------------------------------------------------------------- pages

  class Response {
    constructor(result) {
      hide(this, { _result: result });
    }
    url() {
      const result = this._result;
      return result && typeof result.url === 'string' ? result.url : '';
    }
    status() {
      return 200;
    }
    statusText() {
      return '';
    }
    ok() {
      return true;
    }
    headers() {
      return {};
    }
    async allHeaders() {
      return {};
    }
    text() {
      throw unsupported(
        'response.text()',
        'read the page with page.content() or page.evaluate(() => document.body.innerText), or fetch inside the page with page.evaluate',
      );
    }
    json() {
      throw unsupported('response.json()', 'read the page with page.evaluate(), or fetch inside the page with page.evaluate');
    }
    toJSON() {
      return this._result;
    }
    toString() {
      return `Response(${this.url()})`;
    }
  }

  class Keyboard {
    constructor(page) {
      hide(this, { _page: page });
    }
    press(key) {
      return this._page._send('press', { key: safeString(key) });
    }
    type(text) {
      return this._page._send('type', { text: safeString(text) });
    }
    insertText(text) {
      return this._page._send('type', { text: safeString(text) });
    }
    down() {
      throw unsupported('keyboard.down()', 'press a combination instead, e.g. keyboard.press("Shift+Tab")');
    }
    up() {
      throw unsupported('keyboard.up()', 'press a combination instead, e.g. keyboard.press("Shift+Tab")');
    }
  }

  const number = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : 0);

  class Mouse {
    constructor(page) {
      hide(this, { _page: page });
    }
    click(x, y, options = {}) {
      options = options || {};
      return this._page._send('click', { x: Number(x), y: Number(y), button: options.button, count: options.clickCount });
    }
    dblclick(x, y, options = {}) {
      options = options || {};
      return this._page._send('click', { x: Number(x), y: Number(y), button: options.button, count: 2 });
    }
    move(x, y) {
      return this._page._send('hover', { x: Number(x), y: Number(y) });
    }
    wheel(dx, dy) {
      return this._page._send('scroll', { dx: number(dx), dy: number(dy) });
    }
    down() {
      throw unsupported('mouse.down()', 'drag with locator.dragTo(target)');
    }
    up() {
      throw unsupported('mouse.up()', 'drag with locator.dragTo(target)');
    }
  }

  class Touchscreen {
    constructor(page) {
      hide(this, { _page: page });
    }
    tap(x, y) {
      return this._page.mouse.click(x, y);
    }
  }

  function globToRegExp(glob) {
    let out = '';
    for (let i = 0; i < glob.length; i++) {
      const c = glob[i];
      if (c === '*' && glob[i + 1] === '*') {
        out += '.*';
        i++;
      } else if (c === '*') {
        out += '[^/]*';
      } else {
        out += c.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
      }
    }
    return new RegExp(`^${out}$`);
  }

  function urlMatcher(pattern) {
    if (typeof pattern === 'function') return async (url) => !!(await pattern(url));
    if (pattern instanceof RegExp) return async (url) => pattern.test(url);
    const text = safeString(pattern);
    if (text.includes('*')) {
      const re = globToRegExp(text);
      return async (url) => re.test(url);
    }
    const bare = (s) => (s.endsWith('/') ? s.slice(0, -1) : s);
    return async (url) => bare(url) === bare(text);
  }

  class Page {
    constructor(tab) {
      hide(this, { _tab: tab, _timeout: undefined });
      hide(this, { _keyboard: new Keyboard(this), _mouse: new Mouse(this), _touchscreen: new Touchscreen(this) });
    }

    toString() {
      return `Page(${this._tab})`;
    }

    _send(name, args) {
      return send(name, Object.assign({}, args, { tab: this._tab }));
    }

    _waitTimeout(value) {
      return timeoutOr(value, this._timeout ?? WAIT_TIMEOUT);
    }

    get keyboard() {
      return this._keyboard;
    }
    get mouse() {
      return this._mouse;
    }
    get touchscreen() {
      return this._touchscreen;
    }

    // Going places
    async goto(url, options = {}) {
      options = options || {};
      if (options.waitUntil !== undefined && options.waitUntil !== 'load') {
        warn(`goto({waitUntil: ${JSON.stringify(options.waitUntil)}}): only the load event is waited for`);
      }
      return new Response(await this._send('navigate', { url: safeString(url) }));
    }
    async reload() {
      return new Response(await this._send('reload', {}));
    }
    async goBack() {
      return new Response(await this._send('back', {}));
    }
    async goForward() {
      return new Response(await this._send('forward', {}));
    }
    url() {
      const info = N.info(this._tab);
      if (info == null) throw new Error(`the tab ${this._tab} is closed`);
      return JSON.parse(info).url;
    }
    isClosed() {
      return N.info(this._tab) == null;
    }

    // Reading
    title() {
      return this._send('eval', { script: 'document.title' });
    }
    content() {
      return this._send('eval', {
        script:
          "(() => { const d = document.doctype; return (d ? new XMLSerializer().serializeToString(d) : '') + document.documentElement.outerHTML; })()",
      });
    }
    evaluate(fn, arg) {
      if (typeof fn === 'string') return this._send('eval', { script: fn });
      if (typeof fn !== 'function') throw new TypeError('page.evaluate needs a function or a string of JavaScript');
      const json = arg === undefined ? undefined : JSON.stringify(arg, bigints);
      return this._send('eval', { script: `(${fn.toString()})(${json === undefined ? 'undefined' : json})` });
    }
    evaluateHandle() {
      throw unsupported('page.evaluateHandle()', 'use page.evaluate() and return plain data');
    }
    $eval(selector, fn, arg) {
      return this.locator(selector).first().evaluate(fn, arg);
    }
    $$eval(selector, fn, arg) {
      return this.locator(selector).evaluateAll(fn, arg);
    }
    async $(selector) {
      const first = this.locator(selector).first();
      return (await first.count()) >= 1 ? first : null;
    }
    $$(selector) {
      return this.locator(selector).all();
    }

    // Locators
    locator(selector, options) {
      return new Locator(this, selectorSteps(selector).concat(filterSteps(options)));
    }
    getByRole(role, options) {
      return new Locator(this, [roleStep(role, options)]);
    }
    getByText(text, options = {}) {
      return new Locator(this, [textStep('text', text, options && options.exact)]);
    }
    getByLabel(text, options = {}) {
      return new Locator(this, [textStep('label', text, options && options.exact)]);
    }
    getByPlaceholder(text, options = {}) {
      return new Locator(this, [textStep('placeholder', text, options && options.exact)]);
    }
    getByAltText(text, options = {}) {
      return new Locator(this, [textStep('alt', text, options && options.exact)]);
    }
    getByTitle(text, options = {}) {
      return new Locator(this, [textStep('title', text, options && options.exact)]);
    }
    getByTestId(id) {
      return new Locator(this, [textStep('testid', id, true)]);
    }
    frameLocator() {
      throw unsupported('page.frameLocator()', "locators already look inside same-origin frames; another site's frames can't be reached");
    }
    frame() {
      throw unsupported('page.frame()', "locators already look inside same-origin frames; another site's frames can't be reached");
    }
    dragAndDrop(source, target, options) {
      return this.locator(source).first().dragTo(this.locator(target).first(), options);
    }

    // The rest of the browser
    context() {
      return context;
    }
    mainFrame() {
      return this;
    }
    frames() {
      return [this];
    }
    setDefaultTimeout(ms) {
      this._timeout = timeoutOr(ms, undefined);
    }
    setDefaultNavigationTimeout() {}

    // Waiting
    waitForTimeout(ms) {
      return delay(ms);
    }
    async waitForSelector(selector, options = {}) {
      options = options || {};
      const locator = this.locator(selector).first();
      const want = options.state === undefined ? 'visible' : options.state;
      await locator.waitFor({ state: want, timeout: this._waitTimeout(options.timeout) });
      return want === 'visible' || want === 'attached' ? locator : null;
    }
    async waitForURL(pattern, options = {}) {
      options = options || {};
      const limit = this._waitTimeout(options.timeout);
      const start = Date.now();
      const matches = urlMatcher(pattern);
      for (;;) {
        const now = this.url();
        if (await matches(now)) break;
        if (Date.now() - start >= limit) {
          throw timeoutError(`page.waitForURL(${formatItem(pattern)}): no match within ${limit} ms; the page is at ${now}`);
        }
        await delay(POLL);
      }
      if (options.waitUntil !== undefined) {
        await this.waitForLoadState(options.waitUntil, { timeout: Math.max(1000, limit - (Date.now() - start)) });
      }
      return undefined;
    }
    async waitForLoadState(state = 'load', options = {}) {
      state = state || 'load';
      const limit = this._waitTimeout(options && options.timeout);
      const ready = state === 'domcontentloaded' ? ['interactive', 'complete'] : ['complete'];
      const start = Date.now();
      for (;;) {
        let now;
        try {
          now = await this._send('eval', { script: 'document.readyState' });
        } catch (_) {
          now = undefined;
        }
        if (ready.includes(now)) break;
        if (Date.now() - start >= limit) {
          throw timeoutError(`page.waitForLoadState(${JSON.stringify(state)}): not reached within ${limit} ms`);
        }
        await delay(POLL);
      }
      if (state === 'networkidle') await delay(500);
      return undefined;
    }
    async waitForNavigation(options = {}) {
      options = options || {};
      const limit = this._waitTimeout(options.timeout);
      const start = Date.now();
      const from = this.url();
      while (this.url() === from && Date.now() - start < limit) await delay(POLL);
      const left = () => Math.max(1000, limit - (Date.now() - start));
      if (options.url !== undefined) await this.waitForURL(options.url, { timeout: left() });
      await this.waitForLoadState('load', { timeout: left() });
      return null;
    }
    async waitForFunction(fn, arg, options = {}) {
      options = options || {};
      const limit = this._waitTimeout(options.timeout);
      const every = typeof options.polling === 'number' && options.polling > 0 ? options.polling : POLL;
      const start = Date.now();
      for (;;) {
        const value = await this.evaluate(fn, arg);
        if (value) return value;
        if (Date.now() - start >= limit) {
          throw timeoutError(`page.waitForFunction: no truthy value within ${limit} ms`);
        }
        await delay(every);
      }
    }
    waitForEvent(event) {
      throw unsupported(`page.waitForEvent(${formatItem(event)})`, 'wait for what the event changes on the page instead (page.waitForURL, locator.waitFor, page.waitForFunction)');
    }
    waitForResponse() {
      throw unsupported('page.waitForResponse()', 'wait for what the response changes on the page instead (locator.waitFor, page.waitForFunction), or record traffic with browse.networkStart()');
    }
    waitForRequest() {
      throw unsupported('page.waitForRequest()', 'wait for what the request changes on the page instead (locator.waitFor, page.waitForURL), or record traffic with browse.networkStart()');
    }

    // Events
    on(event, handler) {
      if (event === 'dialog') {
        const source = typeof handler === 'function' ? Function.prototype.toString.call(handler) : '';
        const dismiss = /dismiss/i.test(source);
        warn(
          `page.on('dialog') handlers are never called: Agentloft answers dialogs for the page. This handler seems to want to ` +
            `${dismiss ? 'dismiss' : 'accept'} them; to answer one yourself, call browse.dialog({accept: ${!dismiss}}) while it is open`,
        );
        return this;
      }
      throw unsupported(`page.on(${formatItem(event)})`, 'page events are not delivered to scripts; poll for the change instead (page.waitForURL, locator.waitFor, page.waitForFunction)');
    }
    once(event, handler) {
      return this.on(event, handler);
    }
    addListener(event, handler) {
      return this.on(event, handler);
    }
    off() {
      return this;
    }
    removeListener() {
      return this;
    }
    removeAllListeners() {
      return this;
    }

    // Pictures and presentation
    async screenshot(options = {}) {
      options = options || {};
      if (options.path !== undefined) warn('screenshot({path}): no file is written; return the picture from the script to see it');
      for (const key of ['mask', 'animations', 'omitBackground']) {
        if (options[key] !== undefined) warn(`screenshot({${key}}) is ignored`);
      }
      return this._send('screenshot', {
        full_page: options.fullPage ? true : undefined,
        clip: options.clip,
        format: options.type,
        quality: options.quality,
        scale: options.scale,
      });
    }
    setViewportSize(size) {
      size = size || {};
      return this._send('emulate', { viewport: { width: Number(size.width), height: Number(size.height) } });
    }
    viewportSize() {
      return this._send('eval', { script: '({ width: window.innerWidth, height: window.innerHeight })' });
    }
    emulateMedia(options = {}) {
      options = options || {};
      return this._send('emulate', {
        color_scheme: options.colorScheme,
        reduced_motion: options.reducedMotion,
        forced_colors: options.forcedColors,
        contrast: options.contrast,
        media: options.media,
      });
    }
    async close() {
      await this._send('tabs', { op: 'close' });
      if (pages.get(this._tab) === this) pages.delete(this._tab);
      return undefined;
    }
    async bringToFront() {
      return undefined;
    }
    async pause() {
      warn('page.pause() does nothing in a script; handoff() lets the user act in the tab');
      return undefined;
    }
    opener() {
      return null;
    }
    video() {
      return null;
    }
    setContent(html) {
      return this._send('eval', {
        script: `((html) => { document.open(); document.write(html); document.close(); return null; })(${JSON.stringify(safeString(html))})`,
      });
    }
    pdf() {
      throw unsupported('page.pdf()', 'take a screenshot instead (page.screenshot({fullPage: true}))');
    }
    route() {
      throw unsupported('page.route()', "requests can't be intercepted from a script");
    }
    async unroute() {
      return undefined;
    }
    setExtraHTTPHeaders() {
      throw unsupported('page.setExtraHTTPHeaders()', "a script can't change request headers");
    }
    addInitScript() {
      throw unsupported('page.addInitScript()', 'run page.evaluate() after navigating instead');
    }
    exposeFunction() {
      throw unsupported('page.exposeFunction()', 'read the page state with page.evaluate() instead');
    }
    exposeBinding() {
      throw unsupported('page.exposeBinding()', 'read the page state with page.evaluate() instead');
    }
    addScriptTag() {
      throw unsupported('page.addScriptTag()', 'run the code with page.evaluate() instead');
    }
    addStyleTag() {
      throw unsupported('page.addStyleTag()', 'add a <style> element with page.evaluate() instead');
    }
  }

  // Element shortcuts: page.<method>(selector, …) acts on the first match.
  for (const name of [
    'click', 'dblclick', 'tap', 'fill', 'type', 'press', 'hover', 'focus', 'check', 'uncheck', 'setChecked',
    'selectOption', 'setInputFiles', 'textContent', 'innerText', 'innerHTML', 'inputValue', 'getAttribute',
    'isVisible', 'isHidden', 'isChecked', 'isEnabled', 'isDisabled', 'isEditable', 'dispatchEvent',
  ]) {
    const shortcut = {
      [name](selector, ...rest) {
        return this.locator(selector).first()[name](...rest);
      },
    }[name];
    Object.defineProperty(Page.prototype, name, { value: shortcut, writable: true, configurable: true, enumerable: false });
  }

  const pages = new Map();
  function pageFor(tab) {
    let found = pages.get(tab);
    if (!found) {
      found = new Page(tab);
      pages.set(tab, found);
    }
    return found;
  }
  const page = pageFor(MAIN_TAB);
  // `p` if it is one of the script's pages, else the main one.
  const own = (p) => (p instanceof Page && pages.get(p._tab) === p ? p : page);

  class Context {
    async newPage() {
      const opened = await page._send('tabs', { op: 'new' });
      const tab = opened && (opened.tab ?? opened.id);
      if (tab === undefined || tab === null) throw new Error('context.newPage(): the browser did not say which tab it opened');
      return pageFor(String(tab));
    }
    pages() {
      return [...pages.values()];
    }
    browser() {
      return browser;
    }
    async close() {
      return undefined;
    }
    setDefaultTimeout(ms) {
      page.setDefaultTimeout(ms);
    }
    setDefaultNavigationTimeout() {}
    clearCookies() {
      throw unsupported('context.clearCookies()', "the user can clear them from Agentloft's Browser tab");
    }
    cookies() {
      throw unsupported('context.cookies()', 'read document.cookie through page.evaluate()');
    }
    addCookies() {
      throw unsupported('context.addCookies()', 'set them from the page with page.evaluate()');
    }
    storageState() {
      throw unsupported('context.storageState()', 'read storage with page.evaluate()');
    }
    grantPermissions() {
      throw unsupported('context.grantPermissions()', "permissions can't be granted from a script");
    }
    route() {
      throw unsupported('context.route()', "requests can't be intercepted from a script");
    }
    on(event) {
      throw unsupported(`context.on(${formatItem(event)})`, 'events are not delivered to scripts; poll for the change instead');
    }
    waitForEvent(event) {
      throw unsupported(`context.waitForEvent(${formatItem(event)})`, 'poll for the change the event would bring instead');
    }
    addInitScript() {
      throw unsupported('context.addInitScript()', 'run page.evaluate() after navigating instead');
    }
    toString() {
      return 'BrowserContext';
    }
  }

  class Browser {
    contexts() {
      return [context];
    }
    newPage() {
      return context.newPage();
    }
    newContext() {
      throw unsupported('browser.newContext()', "there is one context, the thread's browser; use context.newPage()");
    }
    async close() {
      return undefined;
    }
    isConnected() {
      return true;
    }
    version() {
      return 'chromium';
    }
    toString() {
      return 'Browser';
    }
  }

  for (const proto of [Page, Locator, Keyboard, Mouse, Touchscreen, Response, Context, Browser]) guard(proto.prototype);

  const context = new Context();
  const browser = new Browser();

  // ---------------------------------------------------- script functions

  const browse = new Proxy(
    {},
    {
      get(_, name) {
        if (typeof name !== 'string' || name === 'then' || name === 'toJSON') return undefined;
        return (args) => send(name, args);
      },
    },
  );

  async function snapshot(options) {
    const target = own(options && options.page);
    const result = await target._send('snapshot', { within: options && options.within });
    return result && typeof result.snapshot === 'string' ? result.snapshot : result;
  }

  function ref(id, onPage) {
    const digits = (typeof id === 'number' || typeof id === 'string') && /\d+/.exec(String(id));
    if (!digits) {
      throw new TypeError(`ref(${formatItem(id)}): give the number of a [ref=N] from the latest snapshot, e.g. ref(12) or ref("[ref=12]")`);
    }
    return new Locator(own(onPage), [], Number(digits[0]));
  }

  async function person(action, field, fallback, message, options) {
    options = options && typeof options === 'object' ? options : {};
    const target = own(options.page);
    const text = message === undefined || message === null || String(message) === '' ? fallback : String(message);
    const args = { [field]: text };
    if (options.timeoutMs !== undefined) args.timeout_secs = Math.max(1, Math.ceil(Number(options.timeoutMs) / 1000));
    const waiting = target._send(action, args);
    if (typeof options.start === 'function') {
      waiting.catch(() => {});
      await options.start();
    }
    return await waiting;
  }
  const handoff = (message, options) =>
    person('handoff', 'reason', 'Please finish this step in the browser, then press Continue.', message, options);
  const demonstrate = (message, options) =>
    person('demonstrate', 'what', 'Please show me how this is done in the browser, then press Done.', message, options);

  Object.assign(globalThis, {
    console, browser, context, page, state, browse, files, snapshot, ref, handoff, demonstrate,
    setTimeout, setInterval, clearTimeout, clearInterval, queueMicrotask, structuredClone, fetch, require,
  });

  // --------------------------------------------------- what the engine calls

  function start(script) {
    const run = (async () => {
      let value = await script(console, browser, context, page, state, browse, files, snapshot, ref, handoff, demonstrate);
      if (typeof value === 'function' && !/^class\b/.test(Function.prototype.toString.call(value))) {
        value = await value(page);
      }
      return value;
    })();
    run.catch(() => {});
    return run;
  }

  // The value as `{text, images}` JSON: the text the agent reads, and the
  // kept pictures it returned.
  function render(value) {
    const OMIT = {};
    const omitted = [];
    const images = [];
    const ancestors = new Set();
    const convert = (x, depth) => {
      switch (typeof x) {
        case 'undefined':
        case 'function':
        case 'symbol':
          return OMIT;
        case 'string':
        case 'boolean':
          return x;
        case 'number':
          return Number.isFinite(x) ? x : null;
        case 'bigint':
          return x.toString();
        default:
          break;
      }
      if (x === null) return null;
      if (depth > 32) {
        omitted.push('nesting deeper than 32 levels');
        return OMIT;
      }
      if (ancestors.has(x)) {
        omitted.push('a circular reference');
        return OMIT;
      }
      if (x instanceof Picture) {
        if (typeof x._index === 'number' && !images.includes(x._index)) images.push(x._index);
        return x.toJSON();
      }
      ancestors.add(x);
      try {
        if (x instanceof Response) return convert(x._result, depth);
        const item = (v) => {
          const c = convert(v, depth + 1);
          return c === OMIT ? null : c;
        };
        if (Array.isArray(x)) return x.map(item);
        if (x instanceof Set) return [...x].map(item);
        if (ArrayBuffer.isView(x) && !(x instanceof DataView)) return Array.from(x.slice(0, 1000), item);
        if (x instanceof Map) {
          const out = Object.create(null);
          for (const [k, v] of x) {
            const c = convert(v, depth + 1);
            if (c !== OMIT) out[safeString(k)] = c;
          }
          return out;
        }
        if (x instanceof Date) return Number.isNaN(x.getTime()) ? null : x.toISOString();
        if (x instanceof Error) return { name: safeString(x.name), message: safeString(x.message) };
        const proto = Object.getPrototypeOf(x);
        if (proto === Object.prototype || proto === null) {
          const out = Object.create(null);
          for (const key of Object.keys(x)) {
            const c = convert(x[key], depth + 1);
            if (c !== OMIT) out[key] = c;
          }
          return out;
        }
        omitted.push(`an instance of another class (${safeString(x)})`);
        return OMIT;
      } finally {
        ancestors.delete(x);
      }
    };
    let text;
    if (value === undefined) {
      text = 'undefined';
    } else if (typeof value === 'string') {
      text = value;
    } else {
      const plain = convert(value, 0);
      text = plain === OMIT ? safeString(value) : JSON.stringify(plain);
    }
    if (omitted.length && value !== undefined) {
      warn(`parts of the result could not be represented as JSON and were left out: ${[...new Set(omitted)].join('; ')}`);
    }
    return JSON.stringify({ text, images });
  }

  function reason(value) {
    return formatItem(value).split('\n')[0];
  }

  function saveState() {
    const json = JSON.stringify(state, bigints);
    return json === undefined ? null : json;
  }

  return { start, settle, render, formatError, reason, saveState };
})
