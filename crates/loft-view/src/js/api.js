// What a view's own script sees: `window.agentloft`. It can only ask; the
// bridge (another world, see bridge.js) decides. __STATE__ and __THEME__ are
// filled in when the page is written.
(() => {
  const call = (kind, value) =>
    document.dispatchEvent(new CustomEvent("agentloft:call", { detail: JSON.stringify({ kind, value }) }));
  if (window.agentloft) {
    window.agentloft.state = __STATE__;
    window.agentloft.theme = __THEME__;
    return;
  }
  const api = {
    /** What `setState` saved last (it survives reloads and other windows). */
    state: __STATE__,
    /** The app's colours: { dark, background, foreground, muted, border, accent, … }. */
    theme: __THEME__,
    /** Remember something (JSON) for the next time this view is opened. */
    setState(value) {
      api.state = value;
      call("state", value);
    },
    /** Say something to the agent as the user. Works from a click or a key press. */
    send(text) {
      call("send", text);
    },
    /** Put text on the clipboard. Works from a click or a key press. */
    copy(text) {
      call("copy", text);
    },
    /** Open a link, or a file of the project. Works from a click or a key press. */
    open(target) {
      call("open", target);
    },
  };
  Object.defineProperty(window, "agentloft", { value: api, enumerable: true });
})();
