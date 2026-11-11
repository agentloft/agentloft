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

  const previous = world.__agentloftBridge;
  if (previous) {
    document.removeEventListener("agentloft:call", previous.onCall, true);
    document.removeEventListener("click", previous.onClick, false);
    document.removeEventListener("submit", previous.onSubmit, false);
    document.removeEventListener("load", previous.measure, true);
    document.removeEventListener("mousemove", previous.onMove, true);
    document.removeEventListener("mouseout", previous.onMove, true);
    document.removeEventListener("focusin", previous.onFocus, true);
    document.removeEventListener("focusout", previous.onFocus, true);
    document.removeEventListener("scroll", previous.onScroll, true);
    document.removeEventListener("mousedown", previous.onDown, true);
    window.removeEventListener("focus", previous.onKeys, false);
    window.removeEventListener("blur", previous.onKeys, false);
    clearInterval(previous.timer);
    if (previous.sizes) previous.sizes.disconnect();
  }

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

  // The pointer the page wants where the mouse is. The viewer looks at a
  // picture of the page, so their own pointer has to be told: an I-beam
  // over text, a hand over a link, whatever the page's CSS asks for.
  const cursorAt = (x, y) => {
    const element = document.elementFromPoint(x, y);
    if (!element) return "default";
    const style = getComputedStyle(element);
    // "url(hand.png) 4 4, pointer": the keyword at the end is the fallback.
    const named = style.cursor.split(",").pop().trim();
    if (named && named !== "auto") return named;
    // `auto` is the browser's own choice: a hand on a link, an I-beam on
    // text that can be selected, an arrow elsewhere.
    if (element.closest("a[href]")) return "pointer";
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
  // Which ways the wheel has something to turn where the pointer is:
  // [left, right, up, down]. A host that scrolls the thread around the page
  // gives the page the wheel only where it can use it.
  const reachAt = (x, y) => {
    const can = [false, false, false, false];
    const root = document.scrollingElement || document.documentElement;
    for (let el = document.elementFromPoint(x, y); el; el = el.parentElement) {
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
  let at = null;
  const tellReach = () => {
    const now = at ? reachAt(at[0], at[1]) : [false, false, false, false];
    if (now.join() !== reach) {
      reach = now.join();
      post({ kind: "reach", value: now });
    }
  };
  // What scrolled may have reached its end (or left it); and a host that
  // shows a picture of part of the page wants a new one.
  let scrolled = 0;
  const onScroll = () => {
    tellReach();
    const now = performance.now();
    if (now - scrolled > 60) {
      scrolled = now;
      post({ kind: "scrolled" });
    }
  };

  let pointer = "";
  const onMove = (event) => {
    // Leaving the page altogether arrives as a `mouseout` to nowhere.
    const left = event.type === "mouseout";
    if (left && event.relatedTarget) return;
    at = left ? null : [event.clientX, event.clientY];
    tellReach();
    const wanted = left ? "default" : cursorAt(event.clientX, event.clientY);
    if (wanted !== pointer) {
      pointer = wanted;
      post({ kind: "cursor", value: wanted });
    }
  };

  // A press says where the pointer is as well as a move does (and a
  // pointer that was already there when the page came in never moved).
  const onDown = (event) => {
    at = [event.clientX, event.clientY];
    tellReach();
  };

  // Whether the page has the keyboard at all (it was clicked into).
  const onKeys = (event) => post({ kind: "focused", value: event.type === "focus" });

  // Whether the keyboard is in a field of the page. A host that shows the
  // page only while the pointer is on it keeps it there while this is so.
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

  document.addEventListener("agentloft:call", onCall, true);
  document.addEventListener("click", onClick, false);
  document.addEventListener("submit", onSubmit, false);
  // Images and fonts arriving change the height without a resize of ours.
  document.addEventListener("load", measure, true);
  document.addEventListener("mousemove", onMove, true);
  document.addEventListener("mouseout", onMove, true);
  document.addEventListener("focusin", onFocus, true);
  document.addEventListener("focusout", onFocus, true);
  document.addEventListener("scroll", onScroll, true);
  document.addEventListener("mousedown", onDown, true);
  window.addEventListener("focus", onKeys, false);
  window.addEventListener("blur", onKeys, false);
  const timer = setInterval(measure, 250);
  world.__agentloftBridge = { onCall, onClick, onSubmit, onMove, onDown, onFocus, onScroll, onKeys, measure, timer, sizes };
  measure();
})();
