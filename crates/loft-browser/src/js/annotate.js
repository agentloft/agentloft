// Annotation mode (runs in Agentloft's isolated JS world; the DOM is shared
// with the page). Hover highlights an element: click it to leave a note, or
// drag over an area to note everything in it. Notes queue in
// `__loftAnnotate.notes` until the app collects them; numbered pins stay on
// the page until `clear()`.
if (!window.__loftAnnotate) {
  const ACCENT = '#7c5cff';
  const FONT = '-apple-system, system-ui, sans-serif';
  const host = document.createElement('div');
  host.setAttribute('data-agentloft', 'annotate');
  const root = host.attachShadow({ mode: 'closed' });
  root.innerHTML = `
    <style>
      .box { position: fixed; pointer-events: none; z-index: 2147483646;
             border: 2px solid ${ACCENT}; border-radius: 4px;
             background: ${ACCENT}1f; transition: all 60ms ease-out; }
      .tag { position: fixed; z-index: 2147483647; pointer-events: none;
             font: 600 11px ${FONT}; color: #fff;
             background: ${ACCENT}; padding: 2px 6px; border-radius: 4px; }
      .marquee { position: fixed; pointer-events: none; z-index: 2147483646;
             border: 1.5px dashed ${ACCENT}; border-radius: 6px; background: ${ACCENT}14; }
      .picked { position: fixed; pointer-events: none; z-index: 2147483645;
             border: 1px solid ${ACCENT}aa; border-radius: 3px; background: ${ACCENT}10; }
      .note { position: fixed; z-index: 2147483647; width: 290px;
              font: 13px ${FONT};
              background: #1c1c21; color: #ededef; border: 1px solid #333;
              border-radius: 12px; padding: 10px; box-shadow: 0 12px 32px #0009; }
      .what { font: 600 11px ${FONT}; color: #a99bff; margin: 0 2px 6px;
              white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
      .note textarea { width: 100%; box-sizing: border-box; min-height: 64px;
              background: #111113; color: inherit; border: 1px solid #3a3a42;
              border-radius: 8px; padding: 6px 8px; font: inherit; resize: vertical; outline: none; }
      .note textarea:focus { border-color: ${ACCENT}; }
      .row { display: flex; justify-content: space-between; align-items: center; margin-top: 8px; }
      .hint { color: #8d8d96; font-size: 11px; }
      .add { background: ${ACCENT}; color: #fff; border: 0; border-radius: 6px;
             padding: 4px 10px; font: 600 12px ${FONT}; cursor: pointer; }
      .mark { position: absolute; z-index: 2147483644; pointer-events: none;
              border: 1.5px dashed ${ACCENT}; border-radius: 5px; }
      .pin { position: absolute; z-index: 2147483645; width: 20px; height: 20px;
             border-radius: 10px 10px 10px 2px; background: ${ACCENT}; color: #fff;
             font: 700 11px ${FONT}; display: flex; box-shadow: 0 2px 6px #0006;
             align-items: center; justify-content: center; pointer-events: none; }
    </style>
    <div class="box" hidden></div><div class="tag" hidden></div><div class="marquee" hidden></div>`;
  const box = root.querySelector('.box');
  const tag = root.querySelector('.tag');
  const marquee = root.querySelector('.marquee');
  let active = false;
  let target = null;
  let note = null;
  let count = 0;
  let drag = null; // { x, y, moved }
  let swallowClick = false;
  let picked = []; // highlight boxes for an area being noted

  const selector = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const parts = [];
    for (let e = el; e && e.nodeType === 1 && parts.length < 4; e = e.parentElement) {
      if (e.id) { parts.unshift('#' + CSS.escape(e.id)); break; }
      let part = e.localName;
      const classes = [...e.classList].filter((c) => !/^\d|[:[\]]/.test(c)).slice(0, 2);
      if (classes.length) part += '.' + classes.map((c) => CSS.escape(c)).join('.');
      const parent = e.parentElement;
      if (parent) {
        const same = [...parent.children].filter((c) => c.localName === e.localName);
        if (same.length > 1) part += `:nth-of-type(${same.indexOf(e) + 1})`;
      }
      parts.unshift(part);
    }
    return parts.join(' > ');
  };
  const label = (el) => {
    const text = (el.innerText || el.value || el.getAttribute('aria-label') || el.alt || '').trim();
    return text.replace(/\s+/g, ' ').slice(0, 80);
  };
  const place = (el) => {
    const r = el.getBoundingClientRect();
    Object.assign(box.style, { left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' });
    box.hidden = false;
    tag.textContent = el.localName + (el.id ? '#' + el.id : '');
    Object.assign(tag.style, { left: r.left + 'px', top: Math.max(0, r.top - 20) + 'px' });
    tag.hidden = false;
  };
  const hideHover = () => { box.hidden = true; tag.hidden = true; target = null; };
  const ours = (e) => e.composedPath().includes(host);
  const clearPicked = () => { picked.forEach((p) => p.remove()); picked = []; };
  const closeNote = () => { if (note) { note.remove(); note = null; } clearPicked(); };

  // The biggest visible elements mostly inside a viewport rect (none
  // inside another): the drag needn't cover an element exactly.
  const elementsIn = (r) => {
    const inside = [];
    for (const el of document.body.querySelectorAll('*')) {
      if (el === host) continue;
      const b = el.getBoundingClientRect();
      const area = b.width * b.height;
      if (area < 64) continue;
      const w = Math.min(b.right, r.right) - Math.max(b.left, r.left);
      const h = Math.min(b.bottom, r.bottom) - Math.max(b.top, r.top);
      if (w <= 0 || h <= 0 || (w * h) / area < 0.6) continue;
      const style = getComputedStyle(el);
      if (style.visibility === 'hidden' || style.display === 'none' || style.opacity === '0') continue;
      inside.push(el);
    }
    return inside.filter((el) => !inside.some((o) => o !== el && o.contains(el))).slice(0, 8);
  };

  // Icons: the svg (or the button/link around it), not one of its paths.
  const meaningful = (el) => {
    const svg = el && el.closest && el.closest('svg');
    if (!svg) return el;
    const parent = svg.parentElement;
    return (parent && parent.closest('a, button, [role="button"]')) || svg;
  };

  // A persistent numbered pin (and outline) at a viewport rect.
  const pinAt = (r, n) => {
    const sx = window.scrollX, sy = window.scrollY;
    const mark = document.createElement('div');
    mark.className = 'mark';
    Object.assign(mark.style, { left: r.left + sx + 'px', top: r.top + sy + 'px', width: r.width + 'px', height: r.height + 'px' });
    const pin = document.createElement('div');
    pin.className = 'pin';
    pin.textContent = String(n);
    Object.assign(pin.style, { left: r.right + sx - 10 + 'px', top: Math.max(0, r.top + sy - 10) + 'px' });
    root.appendChild(mark);
    root.appendChild(pin);
  };

  // Open the note box under `r`; `make(text)` returns the note's record.
  const openNote = (r, what, make) => {
    closeNote();
    note = document.createElement('div');
    note.className = 'note';
    note.innerHTML = `<div class="what"></div><textarea placeholder="What should change here?"></textarea>
      <div class="row"><span class="hint">⏎ add · esc cancel</span><button class="add">Add note</button></div>`;
    note.querySelector('.what').textContent = what;
    const top = r.bottom + 8 + 160 < window.innerHeight ? r.bottom + 8 : Math.max(8, r.top - 168);
    const left = Math.min(window.innerWidth - 310, Math.max(8, r.left));
    Object.assign(note.style, { top: Math.max(8, top) + 'px', left: Math.max(8, left) + 'px' });
    root.appendChild(note);
    const area = note.querySelector('textarea');
    const save = () => {
      const text = area.value.trim();
      if (text) {
        count += 1;
        pinAt(r, count);
        api.notes.push(Object.assign(make(text), { n: count, url: location.href, title: document.title }));
      }
      closeNote();
      hideHover();
    };
    note.querySelector('.add').addEventListener('click', (ev) => { ev.stopPropagation(); save(); });
    area.addEventListener('keydown', (ev) => {
      ev.stopPropagation();
      if (ev.key === 'Enter' && !ev.shiftKey) { ev.preventDefault(); save(); }
      if (ev.key === 'Escape') { ev.preventDefault(); closeNote(); }
    });
    setTimeout(() => area.focus(), 0);
  };

  const rectFrom = (a, b) => {
    const left = Math.min(a.x, b.x), top = Math.min(a.y, b.y);
    const width = Math.abs(a.x - b.x), height = Math.abs(a.y - b.y);
    return { left, top, width, height, right: left + width, bottom: top + height };
  };

  const onDown = (e) => {
    if (!active || ours(e) || e.button !== 0) return;
    e.preventDefault(); e.stopPropagation();
    drag = { x: e.clientX, y: e.clientY, moved: false };
  };
  const onMove = (e) => {
    if (!active || ours(e)) return;
    if (drag && (e.buttons & 1)) {
      if (!drag.moved && Math.hypot(e.clientX - drag.x, e.clientY - drag.y) < 6) return;
      drag.moved = true;
      hideHover();
      const r = rectFrom(drag, { x: e.clientX, y: e.clientY });
      Object.assign(marquee.style, { left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' });
      marquee.hidden = false;
      return;
    }
    if (note) return;
    const el = meaningful(document.elementFromPoint(e.clientX, e.clientY));
    if (el && el !== target) { target = el; place(el); }
  };
  const onUp = (e) => {
    if (!active || ours(e) || !drag) return;
    e.preventDefault(); e.stopPropagation();
    const moved = drag.moved;
    const r = rectFrom(drag, { x: e.clientX, y: e.clientY });
    drag = null;
    marquee.hidden = true;
    if (!moved) return; // a click: handled by onClick
    swallowClick = true;
    setTimeout(() => { swallowClick = false; }, 0);
    const els = elementsIn(r);
    const what = els.length
      ? `Area · ${els.length} element${els.length === 1 ? '' : 's'}`
      : `Area · ${Math.round(r.width)} × ${Math.round(r.height)}`;
    openNote(r, what, (text) => ({
      kind: 'area',
      tag: 'area',
      note: text,
      selector: els.map(selector).join(', '),
      label: (() => { const t = els.map(label).filter(Boolean).join(' · ');
                      return t.length > 120 ? t.slice(0, 119).trimEnd() + '…' : t; })(),
      rect: { x: Math.round(r.left + window.scrollX), y: Math.round(r.top + window.scrollY),
              width: Math.round(r.width), height: Math.round(r.height) },
    }));
    for (const el of els) {
      const b = el.getBoundingClientRect();
      const p = document.createElement('div');
      p.className = 'picked';
      Object.assign(p.style, { left: b.left + 'px', top: b.top + 'px', width: b.width + 'px', height: b.height + 'px' });
      root.appendChild(p);
      picked.push(p);
    }
  };
  const onClick = (e) => {
    if (!active || ours(e)) return;
    e.preventDefault(); e.stopPropagation(); e.stopImmediatePropagation();
    if (swallowClick) { swallowClick = false; return; }
    const el = target || meaningful(document.elementFromPoint(e.clientX, e.clientY));
    if (!el) return;
    const r = el.getBoundingClientRect();
    const name = label(el);
    openNote(r, el.localName + (name ? ` · ${name}` : ''), (text) => ({
      kind: 'element', selector: selector(el), note: text, label: name, tag: el.localName,
    }));
  };
  const onKey = (e) => {
    if (active && e.key === 'Escape' && !ours(e)) closeNote();
  };
  const api = {
    notes: [],
    start() {
      if (active) return;
      active = true;
      (document.body || document.documentElement).appendChild(host);
      document.addEventListener('mousemove', onMove, true);
      document.addEventListener('click', onClick, true);
      document.addEventListener('mousedown', onDown, true);
      document.addEventListener('mouseup', onUp, true);
      document.addEventListener('keydown', onKey, true);
    },
    stop() {
      active = false;
      drag = null;
      closeNote();
      hideHover();
      marquee.hidden = true;
      document.removeEventListener('mousemove', onMove, true);
      document.removeEventListener('click', onClick, true);
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('mouseup', onUp, true);
      document.removeEventListener('keydown', onKey, true);
      // Pins stay (until `clear`) so the page still shows what was noted.
    },
    // Remove pins and outlines; numbering starts over.
    clear() {
      root.querySelectorAll('.pin, .mark').forEach((n) => n.remove());
      count = 0;
      api.notes = [];
      if (!active) host.remove();
    },
    take() { const n = api.notes; api.notes = []; return n; },
  };
  window.__loftAnnotate = api;
}
