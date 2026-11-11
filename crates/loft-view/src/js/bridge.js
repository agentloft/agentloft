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
  const timer = setInterval(measure, 250);
  world.__agentloftBridge = { onCall, onClick, onSubmit, measure, timer, sizes };
  measure();
})();
