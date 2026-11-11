// Runs in a world of its own inside a view's page: the page's script can
// neither see nor change anything here. It is the only thing that can reach
// Agentloft (through the `__agentloft` binding, which exists in this world
// alone), so what a page may ask for, and when, is decided here.
//
// Installed again after every `document.open()`, which drops listeners.
(() => {
  const world = globalThis;
  const post = (message) => {
    try {
      world.__agentloft(JSON.stringify(message));
    } catch (_) {
      // The binding goes away with the page.
    }
  };
  // A click or a key the user really made, moments ago. The browser keeps
  // this; a page cannot fake it.
  const gesture = () => !!(navigator.userActivation && navigator.userActivation.isActive);

  // Whether the user has the page itself in front of them (a web view of
  // the app's own) or pictures of it. With the page itself the pointer's
  // shape is the system's business, and nothing here looks at where the
  // pointer is as it moves.
  const real = world.__agentloftReal === true;

  // Everything an earlier run of this script left behind goes first.
  if (world.__agentloftBridge) world.__agentloftBridge.off();
  const heard = [];
  const listen = (target, type, handler, capture) => {
    target.addEventListener(type, handler, capture);
    heard.push([target, type, handler, capture]);
  };

  const onCall = (event) => {
    let call;
    try {
      call = JSON.parse(event.detail);
    } catch (_) {
      return;
    }
    if (!call || typeof call.kind !== "string") return;
    switch (call.kind) {
      case "state":
        post({ kind: "state", value: call.value === undefined ? null : call.value });
        break;
      case "send":
      case "copy":
      case "open":
        // Only on the user's own click or key: a page can't speak for
        // them, or open things, by itself.
        if (!gesture()) {
          post({ kind: "refused", what: call.kind });
          return;
        }
        post({ kind: call.kind, text: String(call.value == null ? "" : call.value) });
        break;
      default:
        break;
    }
  };

  // A link leads out of the view: it opens for the user, the view stays.
  const onClick = (event) => {
    if (event.defaultPrevented) return;
    const link = event.target && event.target.closest ? event.target.closest("a[href]") : null;
    if (!link) return;
    const href = link.getAttribute("href") || "";
    if (href.startsWith("#") || href.toLowerCase().startsWith("javascript:")) return;
    event.preventDefault();
    post({ kind: "open", text: link.href });
  };

  // A form nobody handles would reload the page; there is nowhere to go.
  const onSubmit = (event) => {
    if (!event.defaultPrevented) event.preventDefault();
  };

  // Pictures only: the pointer the page wants where the mouse is. The
  // viewer's own pointer is over a picture and has to be told: an I-beam
  // over text, a hand over a link, whatever the page's CSS asks for.
  const cursorAt = (x, y) => {
    const element = document.elementFromPoint(x, y);
    if (!element) return "default";
    const style = getComputedStyle(element);
    // "url(hand.png) 4 4, pointer": the keyword at the end is the fallback.
    const named = style.cursor.split(",").pop().trim();
    if (named && named !== "auto") return named;
    // `auto` is the browser's own choice: a hand on a link, an I-beam in a
    // field you type in and on text that can be selected, an arrow
    // elsewhere. (Chrome says `text` for a field itself.)
    if (element.closest("a[href]")) return "pointer";
    const field =
      element.tagName === "TEXTAREA" ||
      (element.tagName === "INPUT" &&
        !/^(button|submit|reset|checkbox|radio|range|color|file|image|hidden)$/.test(element.type));
    if (field) return element.disabled ? "default" : "text";
    if (element.isContentEditable) return "text";
    if (style.userSelect === "none") return "default";
    const caret = document.caretRangeFromPoint ? document.caretRangeFromPoint(x, y) : null;
    const node = caret ? caret.startContainer : null;
    if (node && node.nodeType === Node.TEXT_NODE) {
      const run = document.createRange();
      run.selectNodeContents(node);
      for (const box of run.getClientRects()) {
        if (x >= box.left && x <= box.right && y >= box.top && y <= box.bottom) return "text";
      }
    }
    return "default";
  };
  let pointer = "";
  const onMove = (event) => {
    // Leaving the page altogether arrives as a `mouseout` to nowhere.
    const left = event.type === "mouseout";
    if (left && event.relatedTarget) return;
    const wanted = left ? "default" : cursorAt(event.clientX, event.clientY);
    if (wanted !== pointer) {
      pointer = wanted;
      post({ kind: "cursor", value: wanted });
    }
  };

  // The page itself only: which ways the wheel has something to turn in
  // what the pointer is on, [left, right, up, down]. The host scrolls the
  // thread around the page, and gives the page the wheel only where it can
  // use it. Worked out when the pointer comes onto something else and when
  // something scrolls, not as the pointer moves.
  const reachOf = (target) => {
    const can = [false, false, false, false];
    const root = document.scrollingElement || document.documentElement;
    const first = target && target.nodeType === Node.ELEMENT_NODE ? target : target && target.parentElement;
    for (let el = first; el; el = el.parentElement) {
      const style = getComputedStyle(el);
      const scrolls = (overflow) => el === root || overflow === "auto" || overflow === "scroll";
      if (scrolls(style.overflowY) && el.scrollHeight > el.clientHeight + 1) {
        if (el.scrollTop > 0) can[2] = true;
        if (el.scrollTop + el.clientHeight < el.scrollHeight - 1) can[3] = true;
      }
      if (scrolls(style.overflowX) && el.scrollWidth > el.clientWidth + 1) {
        if (el.scrollLeft > 0) can[0] = true;
        if (el.scrollLeft + el.clientWidth < el.scrollWidth - 1) can[1] = true;
      }
    }
    return can;
  };
  let reach = "";
  let under = null;
  const tellReach = () => {
    const now = under && under.isConnected ? reachOf(under) : [false, false, false, false];
    if (now.join() !== reach) {
      reach = now.join();
      post({ kind: "reach", value: now });
    }
  };
  // (A press says what the pointer is on as well: one that was already
  // there when the page came in never came onto anything.)
  const onOver = (event) => {
    under = event.target;
    tellReach();
  };
  const onOut = (event) => {
    if (event.relatedTarget) return;
    under = null;
    tellReach();
  };
  // What scrolled may have reached its end (or left it); and where the
  // host shows a picture of part of the page, it wants a new one.
  let scrolled = 0;
  const onScroll = () => {
    tellReach();
    const now = performance.now();
    if (now - scrolled > 60) {
      scrolled = now;
      post({ kind: "scrolled" });
    }
  };

  // The page itself only: it changed by its own doing (a timer, an answer
  // that came in). The host shows a picture of it most of the time, and
  // wants a new one. At most five times a second.
  let told = 0;
  let telling = 0;
  const changes =
    real && typeof MutationObserver === "function"
      ? new MutationObserver(() => {
          if (telling) return;
          telling = setTimeout(() => {
            telling = 0;
            told = performance.now();
            post({ kind: "changed" });
          }, Math.max(0, 200 - (performance.now() - told)));
        })
      : null;

  // Whether the page has the keyboard at all (it was clicked into).
  const onKeys = (event) => post({ kind: "focused", value: event.type === "focus" });

  // Whether the keyboard is in a field of the page. The host shows the page
  // only while the pointer is on it, and keeps it there while this is so.
  const typable = (element) =>
    !!element &&
    (element.isContentEditable ||
      element.tagName === "TEXTAREA" ||
      element.tagName === "SELECT" ||
      (element.tagName === "INPUT" &&
        !/^(button|submit|reset|checkbox|radio|range|color|file|image)$/.test(element.type)));
  let typing = false;
  const onFocus = (event) => {
    const now = event.type === "focusin" && typable(event.target);
    if (now !== typing) {
      typing = now;
      post({ kind: "typing", value: now });
    }
  };

  let last = -1;
  let observed = null;
  let sizes = null;
  const measure = () => {
    const root = document.documentElement;
    if (!root) return;
    const body = document.body;
    if (sizes && observed !== root) {
      sizes.disconnect();
      sizes.observe(root);
      if (body) sizes.observe(body);
      observed = body ? root : null;
    }
    let height = root.getBoundingClientRect().height;
    if (body) {
      const style = getComputedStyle(body);
      const margins = (parseFloat(style.marginTop) || 0) + (parseFloat(style.marginBottom) || 0);
      height = Math.max(height, body.scrollHeight + margins);
    }
    height = Math.ceil(height);
    if (height !== last) {
      last = height;
      post({ kind: "size", height });
    }
  };
  if (typeof ResizeObserver === "function") sizes = new ResizeObserver(measure);

  listen(document, "agentloft:call", onCall, true);
  listen(document, "click", onClick, false);
  listen(document, "submit", onSubmit, false);
  // Images and fonts arriving change the height without a resize of ours.
  listen(document, "load", measure, true);
  if (real) {
    listen(document, "mouseover", onOver, true);
    listen(document, "mousedown", onOver, true);
    listen(document, "mouseout", onOut, true);
    listen(document, "scroll", onScroll, true);
    listen(document, "focusin", onFocus, true);
    listen(document, "focusout", onFocus, true);
    listen(window, "focus", onKeys, false);
    listen(window, "blur", onKeys, false);
    // (The document stays the same one through `document.open()`.)
    if (changes) changes.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
  } else {
    listen(document, "mousemove", onMove, true);
    listen(document, "mouseout", onMove, true);
  }
  const timer = setInterval(measure, 250);
  world.__agentloftBridge = {
    off: () => {
      for (const [target, type, handler, capture] of heard) target.removeEventListener(type, handler, capture);
      clearInterval(timer);
      clearTimeout(telling);
      if (sizes) sizes.disconnect();
      if (changes) changes.disconnect();
    },
  };
  measure();
})();
