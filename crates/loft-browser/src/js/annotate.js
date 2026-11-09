// Annotation mode (runs in Agentloft's isolated JS world; the DOM is shared
// with the page). Hover highlights an element; click it to leave a note.
// Notes queue in `__loftAnnotate.notes` until the app collects them.
if (!window.__loftAnnotate) {
  const ACCENT = '#7c5cff';
  const host = document.createElement('div');
  host.setAttribute('data-agentloft', 'annotate');
  const root = host.attachShadow({ mode: 'closed' });
  root.innerHTML = `
    <style>
      .box { position: fixed; pointer-events: none; z-index: 2147483646;
             border: 2px solid ${ACCENT}; border-radius: 4px;
             background: ${ACCENT}1f; transition: all 60ms ease-out; }
      .tag { position: fixed; z-index: 2147483647; pointer-events: none;
             font: 600 11px -apple-system, system-ui, sans-serif; color: #fff;
             background: ${ACCENT}; padding: 2px 6px; border-radius: 4px; }
      .note { position: fixed; z-index: 2147483647; width: 280px;
              font: 13px -apple-system, system-ui, sans-serif;
              background: #1c1c21; color: #ededef; border: 1px solid #333;
              border-radius: 10px; padding: 10px; box-shadow: 0 10px 30px #0008; }
      .note textarea { width: 100%; box-sizing: border-box; min-height: 64px;
              background: #111113; color: inherit; border: 1px solid #3a3a42;
              border-radius: 6px; padding: 6px 8px; font: inherit; resize: vertical; outline: none; }
      .note textarea:focus { border-color: ${ACCENT}; }
      .row { display: flex; justify-content: space-between; align-items: center; margin-top: 8px; }
      .hint { color: #8d8d96; font-size: 11px; }
      .add { background: ${ACCENT}; color: #fff; border: 0; border-radius: 6px;
             padding: 4px 10px; font: 600 12px -apple-system, system-ui, sans-serif; cursor: pointer; }
      .pin { position: absolute; z-index: 2147483645; width: 20px; height: 20px;
             border-radius: 10px 10px 10px 2px; background: ${ACCENT}; color: #fff;
             font: 700 11px -apple-system, system-ui, sans-serif; display: flex;
             align-items: center; justify-content: center; pointer-events: none; }
    </style>
    <div class="box" hidden></div><div class="tag" hidden></div>`;
  const box = root.querySelector('.box');
  const tag = root.querySelector('.tag');
  let active = false;
  let target = null;
  let note = null;
  let count = 0;

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
  const ours = (e) => e.composedPath().includes(host);
  const onMove = (e) => {
    if (!active || note || ours(e)) return;
    const el = document.elementFromPoint(e.clientX, e.clientY);
    if (el && el !== target) { target = el; place(el); }
  };
  const closeNote = () => { if (note) { note.remove(); note = null; } };
  const onClick = (e) => {
    if (!active || ours(e)) return;
    e.preventDefault(); e.stopPropagation(); e.stopImmediatePropagation();
    const el = target || document.elementFromPoint(e.clientX, e.clientY);
    if (!el) return;
    closeNote();
    const r = el.getBoundingClientRect();
    note = document.createElement('div');
    note.className = 'note';
    note.innerHTML = `<textarea placeholder="What should change here?"></textarea>
      <div class="row"><span class="hint">⏎ add · esc cancel</span><button class="add">Add note</button></div>`;
    const top = Math.min(window.innerHeight - 150, r.bottom + 8);
    const left = Math.min(window.innerWidth - 300, Math.max(8, r.left));
    Object.assign(note.style, { top: Math.max(8, top) + 'px', left: left + 'px' });
    root.appendChild(note);
    const area = note.querySelector('textarea');
    const save = () => {
      const text = area.value.trim();
      if (text) {
        count += 1;
        const pin = document.createElement('div');
        pin.className = 'pin';
        pin.textContent = String(count);
        const rr = el.getBoundingClientRect();
        Object.assign(pin.style, { left: rr.right + window.scrollX - 10 + 'px', top: rr.top + window.scrollY - 10 + 'px' });
        root.appendChild(pin);
        api.notes.push({ selector: selector(el), note: text, label: label(el),
                         tag: el.localName, url: location.href, title: document.title });
      }
      closeNote();
    };
    note.querySelector('.add').addEventListener('click', (ev) => { ev.stopPropagation(); save(); });
    area.addEventListener('keydown', (ev) => {
      ev.stopPropagation();
      if (ev.key === 'Enter' && !ev.shiftKey) { ev.preventDefault(); save(); }
      if (ev.key === 'Escape') { ev.preventDefault(); closeNote(); }
    });
    setTimeout(() => area.focus(), 0);
  };
  const block = (e) => { if (active && !ours(e)) { e.preventDefault(); e.stopPropagation(); } };
  const api = {
    notes: [],
    start() {
      if (active) return;
      active = true;
      (document.body || document.documentElement).appendChild(host);
      document.addEventListener('mousemove', onMove, true);
      document.addEventListener('click', onClick, true);
      document.addEventListener('mousedown', block, true);
      document.addEventListener('mouseup', block, true);
    },
    stop() {
      active = false;
      closeNote();
      box.hidden = true; tag.hidden = true;
      document.removeEventListener('mousemove', onMove, true);
      document.removeEventListener('click', onClick, true);
      document.removeEventListener('mousedown', block, true);
      document.removeEventListener('mouseup', block, true);
      host.remove();
    },
    take() { const n = api.notes; api.notes = []; return n; },
  };
  window.__loftAnnotate = api;
}
