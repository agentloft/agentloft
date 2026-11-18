// Runs first in every document of a view's page, frames included, in the
// page's own world: the browser runs it as each document's script context is
// made, before anything of the page's (`Page.addScriptToEvaluateOnNewDocument`;
// frames in a process of their own get it over their own session).
//
// WebRTC is how a page would get past every other wall a view has: its
// connections (STUN, TURN, data channels) are neither requests `Fetch` sees
// nor anything `connect-src` governs. So a view has none of it. Every
// interface of WebRTC's (`RTC…`, and the old `webkitRTC…`) is taken off the
// window and put back as a constant `undefined` the page can neither delete
// nor redefine. A frame the page makes gets the same before the page can
// reach into it, so a fresh frame's `contentWindow` is no way around this.
// (Not a sandboxed `srcdoc` frame: Chrome runs it in a process of its own
// before DevTools can put this in. See ADR-0018, "What's left".)
//
// The daemon's browser only: the app's own web view turns WebRTC off with
// WebKit's switch for it (`loft-browser`).
(() => {
  const webrtc = /^(webkit)?RTC/;
  // Names that may not be listed, in some build or other.
  const known = [
    "RTCPeerConnection",
    "webkitRTCPeerConnection",
    "RTCDataChannel",
    "RTCSessionDescription",
    "RTCIceCandidate",
    "RTCRtpSender",
    "RTCRtpReceiver",
    "RTCRtpTransceiver",
    "RTCDtlsTransport",
    "RTCIceTransport",
    "RTCSctpTransport",
    "RTCEncodedAudioFrame",
    "RTCEncodedVideoFrame",
    "RTCCertificate",
    "RTCDTMFSender",
    "RTCRtpScriptTransform",
  ];
  const names = new Set(known);
  // The window's own properties, and those of every prototype behind it.
  for (let at = globalThis; at; at = Object.getPrototypeOf(at)) {
    for (const name of Object.getOwnPropertyNames(at)) {
      if (webrtc.test(name)) names.add(name);
    }
  }
  for (const name of names) {
    for (let at = Object.getPrototypeOf(globalThis); at; at = Object.getPrototypeOf(at)) {
      try {
        delete at[name];
      } catch (_) {
        // Not there, or not ours to delete: the window's own constant
        // below hides it.
      }
    }
    try {
      delete globalThis[name];
    } catch (_) {
      // Already made constant (this document ran us before).
    }
    try {
      Object.defineProperty(globalThis, name, {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false,
      });
    } catch (_) {
      // Already constant.
    }
  }

  // A view opens no windows (its policy's sandbox has no `allow-popups`): a
  // window would be a page without these walls. `window.open(url)` from a
  // click still opens a web address for the user, as `agentloft.open` does
  // (the bridge checks the click).
  const open = (url) => {
    let href;
    try {
      href = new URL(String(url == null ? "" : url), location.href);
    } catch (_) {
      return null;
    }
    if (href.protocol !== "http:" && href.protocol !== "https:") return null;
    let into = document;
    try {
      into = top.document;
    } catch (_) {
      // A frame of another origin: there is no bridge to tell.
    }
    into.dispatchEvent(
      new CustomEvent("agentloft:call", { detail: JSON.stringify({ kind: "open", value: href.href }) }),
    );
    return null;
  };
  try {
    Object.defineProperty(globalThis, "open", { value: open, writable: true, configurable: true });
  } catch (_) {
    // Already ours.
  }
})();
