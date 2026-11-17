// The website's app page (/app, /link, /m/<id>): sign in, your computers,
// and linking one. The landing page at / is index.html.
// A file, not an inline script, so the page runs under a strict CSP.

const $ = (id) => document.getElementById(id);
const screens = ["loading", "welcome", "machines", "opening", "link", "link-done"];

function show(screen) {
  for (const id of screens) $(id).hidden = id !== screen;
  $("top").hidden = screen === "welcome" || screen === "loading";
}

async function api(path, options = {}) {
  const init = { cache: "no-store", ...options };
  if (options.body !== undefined) {
    init.method = "POST";
    init.headers = { "Content-Type": "application/json" };
    init.body = JSON.stringify(options.body);
  }
  const reply = await fetch(path, init);
  let data = {};
  try {
    data = await reply.json();
  } catch {}
  return { status: reply.status, ok: reply.ok, data };
}

function signIn(next) {
  location.href = `/auth/google?next=${encodeURIComponent(next)}`;
}

/** "just now", "5 min ago", "3 h ago", "2 d ago". */
function ago(ms) {
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value);
  }
  for (const child of children) if (child != null) node.append(child);
  return node;
}

// -- account ------------------------------------------------------------------

async function me() {
  const { ok, data } = await api("/api/me");
  if (!ok) return null;
  $("email").textContent = data.email;
  if (data.picture) {
    $("avatar").src = data.picture;
    $("avatar").hidden = false;
  }
  return data;
}

$("sign-out").addEventListener("click", async () => {
  await api("/api/logout", { body: {} });
  location.href = "/";
});

$("delete-account").addEventListener("click", async () => {
  const sure = confirm(
    "Delete your Agentloft account?\n\nYour sign-in and your linked computers are removed from this website. " +
      "Agentloft on your computers keeps working; they just can't be opened from here until you link them again.",
  );
  if (!sure) return;
  const reply = await api("/api/account/delete", { body: { confirm: "delete" } });
  if (reply.ok) {
    location.href = "/";
  } else {
    $("machines-error").textContent = `Couldn't delete the account (${reply.data.error ?? reply.status}). Try again.`;
  }
});

// -- your computers -------------------------------------------------------------

async function open(machine, button) {
  // A public computer opens here, in a frame, so this tab keeps this
  // site's address (and Back returns to the list).
  if (machine.framed) {
    location.href = `/m/${encodeURIComponent(machine.id)}`;
    return;
  }
  button.disabled = true;
  $("machines-error").textContent = "";
  const { ok, status, data } = await api(`/api/machines/${machine.id}/open`, { body: {} });
  button.disabled = false;
  if (ok) {
    location.href = data.url;
  } else if (status === 409) {
    $("machines-error").textContent =
      `${machine.name} hasn't said where to reach it. Turn on Public through Cloudflare in its Settings ▸ Web access.`;
  } else if (status === 401) {
    signIn("/app");
  } else {
    $("machines-error").textContent = "Couldn't open it. Try again.";
  }
}

async function remove(machine) {
  if (!confirm(`Remove ${machine.name} from your account? Link it again from its Settings to bring it back.`)) {
    return;
  }
  await api(`/api/machines/${machine.id}/remove`, { body: {} });
  await loadMachines();
}

function machineRow(machine) {
  let host = "";
  try {
    host = machine.url ? new URL(machine.url).host : "";
  } catch {}
  const status = machine.online
    ? el("span", { class: "pill on" }, "Online")
    : el("span", { class: "pill" }, `Seen ${ago(machine.last_seen_ms)}`);
  const openButton = el("button", { type: "button", class: "primary" }, "Open");
  openButton.addEventListener("click", () => open(machine, openButton));
  if (!machine.url) openButton.disabled = true;
  return el(
    "li",
    { class: "machine" },
    el("img", { class: "glyph", src: "/computer.svg", alt: "", width: "22", height: "22" }),
    el(
      "div",
      { class: "about" },
      el("div", { class: "name" }, machine.name, " ", status),
      el(
        "div",
        { class: "detail muted" },
        host || "No address yet",
        machine.version ? ` · Agentloft ${machine.version.split("+")[0]}` : "",
      ),
    ),
    el(
      "div",
      { class: "actions" },
      openButton,
      el("button", { type: "button", class: "ghost", onclick: () => remove(machine) }, "Remove"),
    ),
  );
}

async function loadMachines() {
  const { ok, status, data } = await api("/api/machines");
  if (status === 401) return signIn("/app");
  if (!ok) {
    $("machines-error").textContent = "Couldn't load your computers. Try again.";
    return;
  }
  const list = $("machine-list");
  list.replaceChildren(...data.machines.map(machineRow));
  $("empty").hidden = data.machines.length > 0;
}

$("refresh").addEventListener("click", loadMachines);

// -- a computer, shown in place -------------------------------------------------

function openingFailed(text) {
  document.body.classList.remove("framed");
  $("frame").hidden = true;
  $("opening-text").textContent = "";
  $("opening-error").textContent = text;
  $("opening-back").hidden = false;
  show("opening");
}

/** Show computer `id` in the frame. Its page signs in with the ticket in
 *  the address it is given (a partitioned cookie keeps it signed in, in
 *  frames under this site only); if the browser won't keep even that, the
 *  page asks to be opened in the whole tab instead. */
async function openHere(id) {
  show("opening");
  const { ok, status, data } = await api(`/api/machines/${encodeURIComponent(id)}/open`, {
    body: {},
  });
  if (status === 401) return signIn(location.pathname);
  if (status === 404) return openingFailed("This computer isn't in your account any more.");
  if (status === 409) {
    return openingFailed(
      "This computer hasn't said where to reach it. Turn on Public through Cloudflare in its Settings ▸ Web access.",
    );
  }
  if (!ok) return openingFailed("Couldn't open it. Try again.");
  if (!data.framed) {
    // Only on its local network: open it in the whole tab.
    location.replace(data.url);
    return;
  }
  const origin = new URL(data.url).origin;
  document.title = `${data.name} · Agentloft`;
  addEventListener("message", async (event) => {
    if (event.source !== $("frame").contentWindow || event.origin !== origin) return;
    if (event.data?.type !== "agentloft:open-top") return;
    // This browser keeps no cookie in a frame: open it in the whole tab.
    const again = await api(`/api/machines/${encodeURIComponent(id)}/open`, { body: {} });
    if (again.ok) location.replace(again.data.url);
    else openingFailed("Couldn't open it. Try again.");
  });
  const frame = $("frame");
  frame.src = data.url;
  frame.hidden = false;
  document.body.classList.add("framed");
  for (const screen of screens) $(screen).hidden = true;
  $("top").hidden = true;
  frame.focus();
}

// -- linking a computer -----------------------------------------------------------

const LINK_KEY = "agentloft-link-key";

/** The key from the link the computer opened (kept across signing in, in
 *  this tab only). */
function linkKey() {
  const match = /(?:^#|&)k=([A-Za-z0-9_-]+)/.exec(location.hash);
  if (match) {
    sessionStorage.setItem(LINK_KEY, match[1]);
    history.replaceState(null, "", location.pathname);
  }
  return sessionStorage.getItem(LINK_KEY);
}

function linkDone(title, text) {
  sessionStorage.removeItem(LINK_KEY);
  $("link-done-title").textContent = title;
  $("link-done-text").textContent = text;
  show("link-done");
}

async function startLink(key) {
  const { ok, status, data } = await api("/api/link/info", { body: { key } });
  if (status === 401) return signIn("/link");
  if (!ok) {
    return linkDone(
      "This link has expired",
      "Choose Link this computer in Agentloft again to get a new one.",
    );
  }
  $("link-name").textContent = data.name;
  $("link-code").textContent = data.user_code;
  show("link");
  const decide = async (action) => {
    $("link-approve").disabled = $("link-deny").disabled = true;
    const reply = await api(`/api/link/${action}`, { body: { key } });
    if (!reply.ok) {
      $("link-approve").disabled = $("link-deny").disabled = false;
      $("link-error").textContent = "That didn't work. Choose Link this computer in Agentloft again.";
      return;
    }
    if (action === "approve") {
      linkDone("Linked", `${data.name} is linked to your account. Agentloft there shows it in a moment; you can close this tab.`);
    } else {
      linkDone("Not linked", `${data.name} wasn't linked.`);
    }
  };
  $("link-approve").onclick = () => decide("approve");
  $("link-deny").onclick = () => decide("deny");
}

// -- start ----------------------------------------------------------------------

const params = new URLSearchParams(location.search);
const error = params.get("error");
if (error) history.replaceState(null, "", location.pathname + location.hash);

const user = await me();
const shown = /^\/m\/([A-Za-z0-9_-]+)$/.exec(location.pathname);
if (shown) {
  if (!user) signIn(location.pathname);
  else await openHere(shown[1]);
} else if (location.pathname === "/link") {
  const key = linkKey();
  if (!key) linkDone("Nothing to link", "Choose Link this computer in Agentloft to link it.");
  else if (!user) signIn("/link");
  else await startLink(key);
} else if (!user) {
  $("welcome-error").textContent = error ?? "";
  show("welcome");
} else {
  show("machines");
  if (error) $("machines-error").textContent = error;
  await loadMachines();
  // Keep "Online" honest while the page stays open.
  setInterval(() => {
    if (!document.hidden) loadMachines();
  }, 60_000);
}
