// Agentloft's website: sign in with Google, see the computers linked to
// your account, and open one (docs/adr/0019-website-and-cloudflare-tunnel.md).
//
// It never carries Agentloft's traffic. A computer is linked once
// (Settings ▸ Web access ▸ Link this computer) and from then on tells this
// site its address, and whether it is public (Cloudflare Tunnel). Opening
// it gives the browser that address with a ticket: a two-minute note
// signed with that computer's own key, which its daemon trades for an
// ordinary session. A public computer is shown in a frame on /m/<id> (the
// page may frame that computer's address and nothing else), so the address
// bar stays here; the frame talks straight to the computer. One reachable
// only on its local network (network access) is opened in the whole tab.
//
// Routes (everything else is a static file in public/: the landing page at
// /, the app page at /app):
//   GET  /auth/google, /auth/google/callback   sign in (then back to /app)
//   GET  /api/me, POST /api/logout
//   GET  /api/machines                          your computers
//   POST /api/machines/:id/open                 → { url, name, framed } with a ticket
//   POST /api/machines/:id/remove
//   POST /api/link/info|approve|deny { key }    the page a computer opened
//   POST /api/link/start, /api/link/poll        a computer linking (no cookie)
//   POST /api/machine/report, /api/machine/unlink   a linked computer (Bearer)
//   POST /api/account/delete    the signed-in account and all it has ({"confirm":"delete"})
//   GET  /link                                  the app page (linking a computer)
//   GET  /m/:id                                 the app page, framing that computer

import {
  jwtClaims,
  makeTicket,
  randomToken,
  seal,
  secretsMatch,
  sha256,
  sha256b64url,
  unseal,
  userCode,
} from "./crypto.js";

const SESSION_COOKIE = "__Host-al_session";
const SEARCH_CONSOLE_FILE = "/google9485b2468894ad69.html";
const OAUTH_COOKIE = "__Host-al_oauth";
const SESSION_DAYS = 30;
const LINK_MINUTES = 10;
const POLL_SECONDS = 3;
const TICKET_SECONDS = 120;
/** Link requests one address may start in an hour. */
const LINKS_PER_HOUR = 20;
/** A computer counts as online if it reported within this long (it reports
 *  every five minutes). */
const ONLINE_MS = 12 * 60 * 1000;
const MAX_BODY = 4096;

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** The signed-in app's page (public/app.html); the landing page is /.
 *  Not exported: a Worker module may only export handlers and functions. */
const APP_PATH = "/app";

export default {
  async fetch(request, env) {
    let response;
    try {
      response = await route(request, env);
    } catch (err) {
      if (err instanceof Reply) {
        response = err.response;
      } else {
        console.error(err?.stack ?? err);
        response = json({ error: "internal" }, 500);
      }
    }
    return harden(response);
  },
};

/** Thrown to answer at once. */
class Reply extends Error {
  constructor(response) {
    super("reply");
    this.response = response;
  }
}

function json(value, status = 200, headers = {}) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "Content-Type": "application/json", ...headers },
  });
}

function fail(status, error, extra = {}) {
  return new Reply(json({ error, ...extra }, status));
}

/** Headers every Worker response carries (static files get theirs from
 *  public/_headers). */
function harden(response) {
  const out = new Response(response.body, response);
  const headers = out.headers;
  headers.set("X-Content-Type-Options", "nosniff");
  headers.set("X-Frame-Options", "DENY");
  headers.set("Referrer-Policy", "no-referrer");
  headers.set("Strict-Transport-Security", "max-age=31536000");
  if (!headers.has("Cache-Control")) headers.set("Cache-Control", "no-store");
  if (!headers.has("Content-Security-Policy")) {
    headers.set("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'");
  }
  return out;
}

async function route(request, env) {
  const url = new URL(request.url);
  const { pathname: path } = url;
  const method = request.method;
  const GET = method === "GET" || method === "HEAD";
  const POST = method === "POST";

  // Google Search Console's proof that this site is ours (needed for the
  // Google sign-in's branding). Answered here at exactly this address: as
  // a static file, the asset server would redirect it to a name without
  // ".html". Keep it, or the site stops being verified.
  if (path === SEARCH_CONSOLE_FILE && GET) {
    return new Response(`google-site-verification: ${SEARCH_CONSOLE_FILE.slice(1)}`, {
      headers: { "Content-Type": "text/html; charset=utf-8" },
    });
  }
  if (path === "/auth/google" && GET) return startGoogle(request, env, url);
  if (path === "/auth/google/callback" && GET) return finishGoogle(request, env, url);

  // A computer, not a browser: no cookies involved.
  if (path === "/api/link/start" && POST) return linkStart(request, env, url);
  if (path === "/api/link/poll" && POST) return linkPoll(request, env);
  if (path === "/api/machine/report" && POST) return machineReport(request, env);
  if (path === "/api/machine/unlink" && POST) return machineUnlink(request, env);

  // A browser: signed in with the session cookie. Whatever changes
  // something must come from this site's own page.
  if (POST) sameOrigin(request, url);
  if (path === "/api/me" && GET) {
    const user = await signedIn(request, env);
    return json({ email: user.email, name: user.name, picture: user.picture });
  }
  if (path === "/api/logout" && POST) return logout(request, env);
  if (path === "/api/account/delete" && POST) return deleteAccount(request, env);
  if (path === "/api/machines" && GET) return listMachines(request, env);
  const machine = /^\/api\/machines\/([A-Za-z0-9_-]{1,64})\/(open|remove)$/.exec(path);
  if (machine && POST) {
    const [, id, action] = machine;
    return action === "open" ? openMachine(request, env, id) : removeMachine(request, env, id);
  }
  const link = /^\/api\/link\/(info|approve|deny)$/.exec(path);
  if (link && POST) return linkDecide(request, env, link[1]);

  const shown = /^\/m\/([A-Za-z0-9_-]{1,64})$/.exec(path);
  if (shown && GET) return machinePage(request, env, shown[1]);
  // The page a linking computer opens (`verify_url`): the app page, whose
  // script reads the key from the fragment.
  if (path === "/link" && GET) return appPage(request, env);

  if (path.startsWith("/api/") || path.startsWith("/auth/")) {
    return json({ error: "not_found" }, 404);
  }
  return env.ASSETS.fetch(request);
}

// -- requests ---------------------------------------------------------------

/** A POST from a browser must come from this site (the session cookie is
 *  SameSite=Lax too; this is the second lock). */
function sameOrigin(request, url) {
  const origin = request.headers.get("Origin");
  const site = request.headers.get("Sec-Fetch-Site");
  if (origin !== url.origin || (site && site !== "same-origin")) {
    throw fail(403, "cross_site");
  }
}

async function body(request) {
  const type = (request.headers.get("Content-Type") ?? "").split(";")[0].trim();
  if (type !== "application/json") throw fail(415, "send_json");
  const text = await request.text();
  if (text.length > MAX_BODY) throw fail(413, "too_large");
  try {
    const value = JSON.parse(text);
    if (value && typeof value === "object" && !Array.isArray(value)) return value;
  } catch {}
  throw fail(400, "bad_request");
}

/** A short, printable string from a request, or `fallback`. */
function clean(value, max, fallback = "") {
  if (typeof value !== "string") return fallback;
  const text = value.replace(/[\u0000-\u001f\u007f]/g, "").trim();
  return text ? text.slice(0, max) : fallback;
}

/** Where a browser should open a computer: an http(s) origin with a host
 *  name and no credentials, path or query. */
export function cleanOrigin(value) {
  if (typeof value !== "string" || value.length > 300) return null;
  let url;
  try {
    url = new URL(value);
  } catch {
    return null;
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") return null;
  if (url.username || url.password || !url.hostname) return null;
  if ((url.pathname !== "/" && url.pathname !== "") || url.search || url.hash) return null;
  // It goes into the /m/ page's `frame-src` as it is: a plain host name or
  // IP literal only. `new URL` also takes `*`, `;` and `,` in a host, which
  // would widen or split that policy.
  const host = url.hostname;
  const plain =
    /^[a-z0-9-]+(\.[a-z0-9-]+)*$/.test(host) || /^\[[0-9a-f:.]+\]$/.test(host);
  if (!plain) return null;
  return url.origin;
}

function cookie(request, name) {
  for (const part of (request.headers.get("Cookie") ?? "").split(";")) {
    const [key, ...rest] = part.trim().split("=");
    if (key === name) return rest.join("=");
  }
  return null;
}

function setCookie(name, value, maxAgeSecs) {
  return `${name}=${value}; Path=/; Max-Age=${maxAgeSecs}; HttpOnly; Secure; SameSite=Lax`;
}

// -- signing in with Google ---------------------------------------------------

/** Only a path on this site: no `//host`, no `/\host`. Signing in lands
 *  on the app unless it says otherwise. */
export function safeNext(next) {
  return typeof next === "string" && /^\/(?![/\\])[\x21-\x7e]*$/.test(next) ? next : APP_PATH;
}

async function startGoogle(request, env, url) {
  if (!env.GOOGLE_CLIENT_ID || !env.GOOGLE_CLIENT_SECRET) {
    return json({ error: "sign_in_not_configured" }, 503);
  }
  const state = randomToken();
  const verifier = randomToken();
  const next = safeNext(url.searchParams.get("next"));
  const google = new URL("https://accounts.google.com/o/oauth2/v2/auth");
  google.search = new URLSearchParams({
    client_id: env.GOOGLE_CLIENT_ID,
    redirect_uri: `${url.origin}/auth/google/callback`,
    response_type: "code",
    scope: "openid email profile",
    state,
    code_challenge: await sha256b64url(verifier),
    code_challenge_method: "S256",
    prompt: "select_account",
  }).toString();
  return new Response(null, {
    status: 302,
    headers: {
      Location: google.toString(),
      // Lax: it has to come back with Google's redirect.
      "Set-Cookie": setCookie(OAUTH_COOKIE, `${state}.${verifier}.${encodeURIComponent(next)}`, 600),
    },
  });
}

/** Back to the app with what went wrong, as a code: the page shows its own
 *  sentence for each, never text taken from its address (anyone can send a
 *  link with `?error=` of their choosing). */
function signInFailed(code) {
  return new Response(null, {
    status: 302,
    headers: {
      Location: `${APP_PATH}?error=${encodeURIComponent(code)}`,
      "Set-Cookie": setCookie(OAUTH_COOKIE, "", 0),
    },
  });
}

async function finishGoogle(request, env, url) {
  const [state, verifier, ...rest] = (cookie(request, OAUTH_COOKIE) ?? "").split(".");
  const next = rest.join(".");
  const given = url.searchParams.get("state") ?? "";
  if (!state || !verifier || !(await secretsMatch(state, given))) {
    return signInFailed("expired");
  }
  if (url.searchParams.get("error")) {
    return signInFailed("cancelled");
  }
  const code = url.searchParams.get("code");
  if (!code) return signInFailed("unfinished");
  const reply = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      code,
      client_id: env.GOOGLE_CLIENT_ID,
      client_secret: env.GOOGLE_CLIENT_SECRET,
      redirect_uri: `${url.origin}/auth/google/callback`,
      grant_type: "authorization_code",
      code_verifier: verifier,
    }),
  });
  if (!reply.ok) {
    console.warn("google token exchange failed", reply.status, await reply.text());
    return signInFailed("unfinished");
  }
  const tokens = await reply.json();
  // Straight from Google's token endpoint over TLS: no signature to check,
  // but it must be for us, from Google, and current.
  const claims = jwtClaims(tokens.id_token);
  const now = Date.now();
  const issuerOk = claims.iss === "https://accounts.google.com" || claims.iss === "accounts.google.com";
  if (claims.aud !== env.GOOGLE_CLIENT_ID || !issuerOk || !(claims.exp * 1000 > now) || !claims.sub) {
    return signInFailed("unchecked");
  }
  if (claims.email_verified !== true || typeof claims.email !== "string") {
    return signInFailed("unverified");
  }
  const email = claims.email.toLowerCase();
  if (!emailAllowed(env, email)) {
    return signInFailed("not_allowed");
  }
  const userId = `g_${claims.sub}`;
  const secret = randomToken();
  await env.DB.batch([
    env.DB.prepare(
      `INSERT INTO users (id, email, name, picture, created_ms, last_login_ms)
       VALUES (?1, ?2, ?3, ?4, ?5, ?5)
       ON CONFLICT(id) DO UPDATE SET email = ?2, name = ?3, picture = ?4, last_login_ms = ?5`,
    ).bind(userId, email, clean(claims.name, 200), clean(claims.picture, 500), now),
    env.DB.prepare(
      "INSERT INTO sessions (hash, user_id, created_ms, expires_ms) VALUES (?1, ?2, ?3, ?4)",
    ).bind(await sha256(secret), userId, now, now + SESSION_DAYS * DAY),
    env.DB.prepare("DELETE FROM sessions WHERE expires_ms < ?1").bind(now),
  ]);
  let to = APP_PATH;
  try {
    to = safeNext(decodeURIComponent(next));
  } catch {}
  const headers = new Headers({ Location: to });
  headers.append("Set-Cookie", setCookie(SESSION_COOKIE, secret, SESSION_DAYS * 86_400));
  headers.append("Set-Cookie", setCookie(OAUTH_COOKIE, "", 0));
  return new Response(null, { status: 302, headers });
}

/** Whether `email` may use the site (`ALLOWED_EMAILS`; empty: everyone). */
function emailAllowed(env, email) {
  const allowed = (env.ALLOWED_EMAILS ?? "")
    .split(",")
    .map((e) => e.trim().toLowerCase())
    .filter(Boolean);
  return !allowed.length || allowed.includes(String(email).toLowerCase());
}

async function signedIn(request, env) {
  const secret = cookie(request, SESSION_COOKIE);
  if (secret) {
    const user = await env.DB.prepare(
      `SELECT users.* FROM sessions JOIN users ON users.id = sessions.user_id
       WHERE sessions.hash = ?1 AND sessions.expires_ms > ?2`,
    )
      .bind(await sha256(secret), Date.now())
      .first();
    // Taken off ALLOWED_EMAILS since signing in: signed out now, not
    // when the session runs out.
    if (user && emailAllowed(env, user.email)) return user;
  }
  throw fail(401, "signed_out");
}

async function logout(request, env) {
  const secret = cookie(request, SESSION_COOKIE);
  if (secret) {
    await env.DB.prepare("DELETE FROM sessions WHERE hash = ?1").bind(await sha256(secret)).run();
  }
  return json({ ok: true }, 200, { "Set-Cookie": setCookie(SESSION_COOKIE, "", 0) });
}

/** Delete the signed-in account and everything kept about it: its
 *  sessions, linked computers (their next report finds them gone, and they
 *  unlink themselves) and pending links. The body must say so, so that no
 *  stray request deletes an account. */
async function deleteAccount(request, env) {
  const user = await signedIn(request, env);
  const input = await body(request);
  if (input?.confirm !== "delete") throw fail(400, "confirm");
  await env.DB.batch([
    env.DB.prepare("DELETE FROM links WHERE user_id = ?1").bind(user.id),
    env.DB.prepare("DELETE FROM machines WHERE user_id = ?1").bind(user.id),
    env.DB.prepare("DELETE FROM sessions WHERE user_id = ?1").bind(user.id),
    env.DB.prepare("DELETE FROM users WHERE id = ?1").bind(user.id),
  ]);
  return json({ ok: true }, 200, { "Set-Cookie": setCookie(SESSION_COOKIE, "", 0) });
}

// -- your computers -----------------------------------------------------------

function machineView(row, now) {
  return {
    id: row.id,
    name: row.name,
    platform: row.platform,
    version: row.version,
    url: row.url,
    framed: framed(row),
    created_ms: row.created_ms,
    last_seen_ms: row.last_seen_ms,
    online: now - row.last_seen_ms < ONLINE_MS,
  };
}

async function listMachines(request, env) {
  const user = await signedIn(request, env);
  const { results } = await env.DB.prepare(
    "SELECT * FROM machines WHERE user_id = ?1 ORDER BY last_seen_ms DESC",
  )
    .bind(user.id)
    .all();
  const now = Date.now();
  return json({ machines: results.map((row) => machineView(row, now)) });
}

async function ownMachine(env, user, id) {
  const row = await env.DB.prepare("SELECT * FROM machines WHERE id = ?1 AND user_id = ?2")
    .bind(id, user.id)
    .first();
  if (!row) throw fail(404, "not_found");
  return row;
}

/** Whether the site shows this computer in a frame, under its own address:
 *  only a public (Cloudflare Tunnel) one, over HTTPS. One on a local
 *  network is plain HTTP, which an HTTPS page can't frame, and would draw
 *  Local Network Access prompts. */
function framed(row) {
  if (!row.public || !row.url) return false;
  try {
    return new URL(row.url).protocol === "https:";
  } catch {
    return false;
  }
}

/** The app page's policy (public/_headers), which frames nothing. */
const PAGE_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https://*.googleusercontent.com; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/** /m/<id>: the page, allowed to frame exactly that computer's address when
 *  it is a public one of the signed-in account's; otherwise nothing (the
 *  page then sends the browser to sign in, or to the computer). */
async function machinePage(request, env, id) {
  let frame = "";
  try {
    const user = await signedIn(request, env);
    const row = await ownMachine(env, user, id);
    if (framed(row) && Date.now() - row.last_seen_ms < ONLINE_MS) frame = `; frame-src ${new URL(row.url).origin}`;
  } catch {}
  return appPage(request, env, frame);
}

/** The app page under another path (/m/<id>, /link), with the page's
 *  headers set here (public/_headers matches the path asked for). */
async function appPage(request, env, frame = "") {
  const page = await env.ASSETS.fetch(new URL(APP_PATH, request.url));
  const out = new Response(page.body, page);
  out.headers.set("Content-Security-Policy", PAGE_CSP + frame);
  out.headers.set("Cross-Origin-Opener-Policy", "same-origin");
  out.headers.set(
    "Permissions-Policy",
    "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
  );
  out.headers.set("Cache-Control", "no-store");
  return out;
}

async function openMachine(request, env, id) {
  const user = await signedIn(request, env);
  const row = await ownMachine(env, user, id);
  if (!row.url) throw fail(409, "no_address");
  // Not heard from lately: its last address may be gone, or (a quick
  // tunnel's random name) be someone else's by now. No ticket goes there.
  if (!(Date.now() - row.last_seen_ms < ONLINE_MS)) throw fail(409, "offline");
  const secret = await unseal(env.SECRETS_KEY, row.secret, row.id);
  const ticket = await makeTicket(secret, {
    machineId: row.id,
    userId: user.id,
    email: user.email,
    audience: row.url,
    now: Date.now(),
    ttl: TICKET_SECONDS,
  });
  // In the fragment: never sent to a server or kept in its logs.
  return json({ url: `${row.url}/#ticket=${ticket}`, name: row.name, framed: framed(row) });
}

async function removeMachine(request, env, id) {
  const user = await signedIn(request, env);
  await env.DB.prepare("DELETE FROM machines WHERE id = ?1 AND user_id = ?2").bind(id, user.id).run();
  return json({ ok: true });
}

// -- linking a computer -------------------------------------------------------

async function linkStart(request, env, url) {
  const input = await body(request);
  const ip = request.headers.get("CF-Connecting-IP") ?? "";
  const now = Date.now();
  await env.DB.prepare("DELETE FROM links WHERE expires_ms < ?1").bind(now - HOUR).run();
  const recent = await env.DB.prepare(
    "SELECT COUNT(*) AS n FROM links WHERE ip = ?1 AND created_ms > ?2",
  )
    .bind(ip, now - HOUR)
    .first("n");
  if (recent >= LINKS_PER_HOUR) throw fail(429, "rate_limited");
  const deviceCode = randomToken();
  const key = randomToken();
  const code = userCode();
  await env.DB.prepare(
    `INSERT INTO links (device_hash, key_hash, user_code, name, platform, ip, created_ms, expires_ms)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)`,
  )
    .bind(
      await sha256(deviceCode),
      await sha256(key),
      code,
      clean(input.name, 80, "A computer"),
      clean(input.platform, 40),
      ip,
      now,
      now + LINK_MINUTES * 60_000,
    )
    .run();
  return json({
    device_code: deviceCode,
    user_code: code,
    // The key goes in the fragment: the browser keeps it, no log does.
    verify_url: `${url.origin}/link#k=${key}`,
    interval: POLL_SECONDS,
    expires_in: LINK_MINUTES * 60,
  });
}

async function pendingLink(env, key) {
  if (typeof key !== "string" || key.length > 100) throw fail(400, "bad_request");
  const row = await env.DB.prepare("SELECT * FROM links WHERE key_hash = ?1")
    .bind(await sha256(key))
    .first();
  if (!row || row.expires_ms < Date.now()) throw fail(410, "expired");
  if (row.state !== "pending") throw fail(409, row.state);
  return row;
}

async function linkDecide(request, env, action) {
  const user = await signedIn(request, env);
  const { key, elsewhere_ok: elsewhereOk } = await body(request);
  const row = await pendingLink(env, key);
  // Device-code phishing: someone starts a link on their own computer and
  // sends the page to a victim. A link approved from another network than
  // the one it was started from needs the page's warning to have been shown.
  const here = request.headers.get("CF-Connecting-IP") ?? "";
  const elsewhere = Boolean(row.ip && here && row.ip !== here);
  if (action === "info") {
    return json({ name: row.name, platform: row.platform, user_code: row.user_code, elsewhere });
  }
  if (action === "approve" && elsewhere && elsewhereOk !== true) {
    throw fail(409, "confirm_elsewhere");
  }
  const state = action === "approve" ? "approved" : "denied";
  const { meta } = await env.DB.prepare(
    "UPDATE links SET state = ?1, user_id = ?2 WHERE device_hash = ?3 AND state = 'pending'",
  )
    .bind(state, user.id, row.device_hash)
    .run();
  if (meta.changes !== 1) throw fail(409, "already_decided");
  return json({ ok: true, name: row.name });
}

/** The computer asks whether it was let in; once it was, it gets its id
 *  and key (once: the request is gone after). */
async function linkPoll(request, env) {
  const { device_code: deviceCode } = await body(request);
  if (typeof deviceCode !== "string" || deviceCode.length > 100) throw fail(400, "bad_request");
  const hash = await sha256(deviceCode);
  const row = await env.DB.prepare("SELECT * FROM links WHERE device_hash = ?1").bind(hash).first();
  const now = Date.now();
  if (!row || row.expires_ms < now) throw fail(410, "expired");
  if (row.state === "denied") throw fail(410, "denied");
  if (row.state !== "approved") return json({ pending: true, interval: POLL_SECONDS }, 202);
  // Collected once: whoever deletes the row hands out the key.
  const { meta } = await env.DB.prepare(
    "DELETE FROM links WHERE device_hash = ?1 AND state = 'approved'",
  )
    .bind(hash)
    .run();
  if (meta.changes !== 1) throw fail(410, "expired");
  const user = await env.DB.prepare("SELECT * FROM users WHERE id = ?1").bind(row.user_id).first();
  if (!user) throw fail(410, "expired");
  const machineId = `m_${randomToken(16)}`;
  const secret = randomToken();
  await env.DB.prepare(
    `INSERT INTO machines (id, user_id, name, platform, secret, created_ms, last_seen_ms)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)`,
  )
    .bind(machineId, user.id, row.name, row.platform, await seal(env.SECRETS_KEY, secret, machineId), now)
    .run();
  return json({ machine_id: machineId, secret, user_id: user.id, email: user.email });
}

// -- a linked computer --------------------------------------------------------

/** The computer behind `Authorization: Bearer <machine id>.<key>`. */
async function machineFromBearer(request, env) {
  const bearer = /^Bearer ([A-Za-z0-9_-]{1,64})\.([A-Za-z0-9_-]{20,100})$/.exec(
    request.headers.get("Authorization") ?? "",
  );
  if (bearer) {
    const [, id, secret] = bearer;
    const row = await env.DB.prepare(
      "SELECT machines.*, users.email AS email FROM machines JOIN users ON users.id = machines.user_id WHERE machines.id = ?1",
    )
      .bind(id)
      .first();
    if (row && (await secretsMatch(secret, await unseal(env.SECRETS_KEY, row.secret, row.id)))) {
      return row;
    }
  }
  throw fail(401, "unlinked");
}

async function machineReport(request, env) {
  const machine = await machineFromBearer(request, env);
  const input = await body(request);
  const url = input.url == null ? null : cleanOrigin(input.url);
  if (input.url != null && !url) throw fail(400, "bad_url");
  await env.DB.prepare(
    `UPDATE machines SET name = ?2, platform = ?3, version = ?4, url = ?5, last_seen_ms = ?6,
     public = ?7 WHERE id = ?1`,
  )
    .bind(
      machine.id,
      clean(input.name, 80, machine.name),
      clean(input.platform, 40, machine.platform),
      clean(input.version, 60, machine.version),
      url,
      Date.now(),
      url && input.public === true ? 1 : 0,
    )
    .run();
  return json({ ok: true, email: machine.email });
}

async function machineUnlink(request, env) {
  const machine = await machineFromBearer(request, env);
  await env.DB.prepare("DELETE FROM machines WHERE id = ?1").bind(machine.id).run();
  return json({ ok: true });
}
