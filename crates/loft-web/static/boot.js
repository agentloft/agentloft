// Signs this browser in, then loads the app (ADR-0016).
//
// The pairing link carries the token in the URL fragment, which browsers
// never send to a server. It is read here, removed from the address bar,
// and traded once (in a POST body) for an HttpOnly session cookie that
// scripts, this one included, cannot read. A file, not an inline script,
// so the page works under a strict Content-Security-Policy.

const boot = document.getElementById("boot");
const pairScreen = document.getElementById("pair");
const input = document.getElementById("pair-input");
const button = document.getElementById("pair-button");
const error = document.getElementById("pair-error");

/** The token in the fragment, if any; the fragment is cleared either way. */
function takeTokenFromAddress() {
  const match = /(?:^#|&)token=([^&]+)/.exec(location.hash);
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  return match ? decodeURIComponent(match[1]) : null;
}

/** A pasted pairing link, or a bare token. */
function tokenFromText(text) {
  const match = /token=([A-Za-z0-9_-]+)/.exec(text);
  return (match ? match[1] : text).trim();
}

async function signedIn() {
  const reply = await fetch("api/session", { cache: "no-store" });
  return reply.ok;
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

async function startApp() {
  boot.textContent = "Loading Agentloft…";
  try {
    // The app says when it has drawn its first frame.
    addEventListener("agentloft-ready", () => boot.remove(), { once: true });
    const { default: init } = await import("./loft_web.js");
    await init({ module_or_path: new URL("./loft_web_bg.wasm", import.meta.url) });
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

const token = takeTokenFromAddress();
try {
  if (token) {
    const problem = await pair(token);
    if (problem) askToPair(problem);
    else await startApp();
  } else if (await signedIn()) {
    await startApp();
  } else {
    askToPair();
  }
} catch {
  boot.textContent = "Couldn't reach Agentloft. Check that it's running, then reload.";
}
