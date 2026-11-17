// Signs this browser in, then loads the app (ADR-0016).
//
// The pairing link carries the token in the URL fragment, which browsers
// never send to a server. It is read here, removed from the address bar,
// and traded once (in a POST body) for an HttpOnly session cookie that
// scripts, this one included, cannot read. A file, not an inline script,
// so the page works under a strict Content-Security-Policy.

const boot = document.getElementById("boot");
const pairTitle = document.getElementById("pair-title");
const pairText = document.getElementById("pair-text");
const pairRow = document.getElementById("pair-row");
const websiteLink = document.getElementById("website-link");
const pairScreen = document.getElementById("pair");
const input = document.getElementById("pair-input");
const button = document.getElementById("pair-button");
const error = document.getElementById("pair-error");

/** Inside the website's frame (it shows a public computer in place, so the
 *  browser's address stays the website's). */
const framed = window.top !== window.self;
// A link to the website must open over the frame, not in it.
if (framed) websiteLink.target = "_top";

/** The pairing token or the website's ticket in the fragment, if any; the
 *  fragment is cleared either way. */
function takeFromAddress() {
  const token = /(?:^#|&)token=([^&]+)/.exec(location.hash);
  const ticket = /(?:^#|&)ticket=([^&]+)/.exec(location.hash);
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  return {
    token: token ? decodeURIComponent(token[1]) : null,
    ticket: ticket ? decodeURIComponent(ticket[1]) : null,
  };
}

/** A pasted pairing link, or a bare token. */
function tokenFromText(text) {
  const match = /token=([A-Za-z0-9_-]+)/.exec(text);
  return (match ? match[1] : text).trim();
}

/** Whether this browser is signed in, and how it may sign in if not: on
 *  the public address only through the website, never pairing. */
async function sessionState() {
  const reply = await fetch("api/session", { cache: "no-store" });
  let data = {};
  try {
    data = await reply.json();
  } catch {}
  return {
    signedIn: reply.ok,
    pairing: data.pairing !== false,
    website: typeof data.website === "string" ? data.website : null,
  };
}

/** The public address: sign in on the website (Google), which sends this
 *  browser back with a ticket. No pairing box. */
function askToSignIn(website, message) {
  boot.hidden = true;
  pairScreen.hidden = false;
  pairTitle.textContent = "Sign in to open Agentloft";
  pairRow.hidden = true;
  if (website) {
    const host = website.replace(/^https?:\/\//, "");
    pairText.textContent = `Sign in at ${host} with your Google account, then open this computer from there.`;
    // `/app` is the signed-in site (sign-in, then your computers); `/` is
    // the landing page.
    websiteLink.href = website.replace(/\/+$/, "") + "/app";
    websiteLink.textContent = `Sign in at ${host}`;
    websiteLink.hidden = false;
  } else {
    pairText.textContent = "Open this computer from Agentloft's website to sign in.";
  }
  error.textContent = message ?? "";
}

async function pair(token) {
  const reply = await fetch("api/pair", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  if (reply.ok) return null;
  if (reply.status === 429) {
    const wait = Number(reply.headers.get("Retry-After")) || 60;
    return `Too many tries. Wait ${wait} seconds, then try again.`;
  }
  if (reply.status === 401) {
    return "That link isn't valid any more. Copy a new one from Settings ▸ Web access.";
  }
  return "Couldn't pair. Check that Agentloft is running, then try again.";
}

/** Sign in with a ticket from the website (Agentloft's own site sends a
 *  signed-in browser here with one, good for two minutes and once). */
async function signInWithTicket(ticket) {
  const reply = await fetch("api/ticket", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ticket, framed }),
  });
  if (reply.ok) return null;
  if (reply.status === 429) {
    const wait = Number(reply.headers.get("Retry-After")) || 60;
    return `Too many tries. Wait ${wait} seconds, then open it from the website again.`;
  }
  let error = "";
  try {
    error = (await reply.json()).error;
  } catch {}
  if (error === "not_linked") {
    return "This computer isn't linked to the website any more. Link it again in Settings ▸ Web access, or pair with a link.";
  }
  return "That sign-in link has expired. Open this computer from the website again.";
}

function askToPair(message) {
  boot.hidden = true;
  pairScreen.hidden = false;
  error.textContent = message ?? "";
  input.focus();
}

async function submit() {
  const token = tokenFromText(input.value);
  if (!token) return;
  button.disabled = true;
  error.textContent = "";
  let problem;
  try {
    problem = await pair(token);
  } catch {
    problem = "Couldn't reach Agentloft. Check that it's running, then try again.";
  }
  button.disabled = false;
  if (problem) {
    error.textContent = problem;
    return;
  }
  input.value = "";
  pairScreen.hidden = true;
  boot.hidden = false;
  await startApp();
}

/** The app's module, counting what has arrived: a first load is several
 *  megabytes, and over a slow link takes seconds. The daemon says the
 *  decoded size (`X-Agentloft-Size`); the stream counts decoded bytes. */
async function appModule() {
  const url = new URL("./loft_web_bg.wasm", import.meta.url);
  const reply = await fetch(url);
  const total = Number(reply.headers.get("X-Agentloft-Size")) || 0;
  if (!reply.ok || !reply.body || !total) return reply;
  let seen = 0;
  let shown = -1;
  const counted = reply.body.pipeThrough(
    new TransformStream({
      transform(chunk, out) {
        seen += chunk.byteLength;
        const percent = Math.min(100, Math.floor((seen / total) * 100));
        if (percent !== shown) {
          shown = percent;
          boot.textContent =
            percent < 100 ? `Loading Agentloft… ${percent}%` : "Starting Agentloft…";
        }
        out.enqueue(chunk);
      },
    }),
  );
  // Still a wasm response, so it compiles while it downloads.
  return new Response(counted, {
    status: reply.status,
    headers: { "Content-Type": reply.headers.get("Content-Type") ?? "application/wasm" },
  });
}

async function startApp() {
  boot.textContent = "Loading Agentloft…";
  try {
    // The app says when it has drawn its first frame.
    addEventListener("agentloft-ready", () => boot.remove(), { once: true });
    const { default: init } = await import("./loft_web.js");
    await init({ module_or_path: appModule() });
  } catch (err) {
    console.error(err);
    boot.textContent = "Agentloft couldn't start in this browser.";
  }
}

// The app asks to be signed out (Settings ▸ Sign out of this browser).
addEventListener("agentloft-sign-out", async () => {
  try {
    await fetch("api/logout", { method: "POST" });
  } finally {
    location.reload();
  }
});

button.addEventListener("click", submit);
input.addEventListener("keydown", (event) => {
  if (event.key === "Enter") submit();
});

/** Signed in with a ticket inside the website's frame, but the browser
 *  kept no cookie for the frame (it blocks them even partitioned): ask the
 *  website to open this computer over the whole tab instead. */
async function cookieKeptInFrame(website) {
  if (!framed || (await sessionState()).signedIn) return true;
  if (website) parent.postMessage({ type: "agentloft:open-top" }, website);
  boot.textContent = "Opening Agentloft…";
  return false;
}

const { token, ticket } = takeFromAddress();
try {
  // Already signed in (a frame the website reopened): the ticket isn't
  // needed, and spending it would only add a session.
  const state = token ? null : await sessionState();
  if (state?.signedIn) {
    await startApp();
  } else if (token || ticket) {
    const problem = token ? await pair(token) : await signInWithTicket(ticket);
    if (problem) {
      const now = await sessionState();
      if (now.pairing) askToPair(problem);
      else askToSignIn(now.website, problem);
    } else if (token || (await cookieKeptInFrame(state.website))) {
      await startApp();
    }
    // Otherwise the website opens this computer over the whole tab.
  } else if (state.pairing) {
    askToPair();
  } else {
    askToSignIn(state.website);
  }
} catch {
  boot.textContent = "Couldn't reach Agentloft. Check that it's running, then reload.";
}
