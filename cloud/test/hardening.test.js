// What the security review of 2026-11-18 tightened: reported addresses, the
// sign-in error shown from the address bar, ALLOWED_EMAILS after sign-in,
// and computers that have gone quiet.
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { cleanOrigin } from "../src/index.js";

const SITE = "https://agentloft.example";
const MINUTE = 60 * 1000;

/** A database that answers every `first()` with `row` and every `all()`
 *  with `rows`. */
function fakeEnv({ row = null, rows = [], allowed = "" } = {}) {
  return {
    GOOGLE_CLIENT_ID: "client-id",
    GOOGLE_CLIENT_SECRET: "client-secret",
    ALLOWED_EMAILS: allowed,
    ASSETS: {
      async fetch(input) {
        const path = new URL(input instanceof Request ? input.url : input).pathname;
        return new Response(`page ${path}`, { headers: { "Content-Type": "text/html" } });
      },
    },
    DB: {
      prepare(sql) {
        const statement = {
          sql,
          bind: () => statement,
          first: async () => (typeof row === "function" ? row(sql) : row),
          all: async () => ({ results: rows }),
          run: async () => ({ meta: { changes: 0 } }),
        };
        return statement;
      },
      batch: async () => [],
    },
  };
}

test("a reported address is a plain host name or IP, nothing a policy would read", () => {
  for (const good of [
    "https://agentloft-desk.example.com",
    "https://able-brook-cat.trycloudflare.com",
    "http://192.168.1.20:4780",
    "http://[fd00::1]:4780",
  ]) {
    assert.equal(cleanOrigin(good), new URL(good).origin, good);
  }
  for (const bad of [
    "https://*",
    "https://*.example.com",
    "https://a;sandbox",
    "https://a,b",
    "https://a%20b",
    "javascript:alert(1)",
    "https://user:pw@example.com",
    "https://example.com/path",
  ]) {
    assert.equal(cleanOrigin(bad), null, bad);
  }
});

test("a failed sign-in sends a code, never a sentence", async () => {
  const response = await worker.fetch(
    new Request(`${SITE}/auth/google/callback?state=x&code=y`, { redirect: "manual" }),
    fakeEnv(),
  );
  assert.equal(response.headers.get("Location"), "/app?error=expired");
});

test("someone taken off ALLOWED_EMAILS is signed out at once", async () => {
  const user = { id: "g_1", email: "gone@example.com" };
  const ask = (allowed) =>
    worker.fetch(
      new Request(`${SITE}/api/machines`, { headers: { Cookie: "__Host-al_session=s" } }),
      fakeEnv({ row: user, allowed }),
    );
  assert.equal((await ask("")).status, 200, "no list: everyone");
  assert.equal((await ask("gone@example.com, other@example.com")).status, 200);
  assert.equal((await ask("other@example.com")).status, 401);
});

test("a computer not heard from lately gets no ticket and no frame", async () => {
  const user = { id: "g_1", email: "me@example.com" };
  const machine = (lastSeen) => ({
    id: "m_1",
    user_id: "g_1",
    name: "Desk",
    url: "https://able-brook-cat.trycloudflare.com",
    public: 1,
    secret: "unused",
    last_seen_ms: lastSeen,
  });
  const env = (lastSeen) =>
    fakeEnv({ row: (sql) => (sql.includes("FROM machines") ? machine(lastSeen) : user) });
  const open = (lastSeen) =>
    worker.fetch(
      new Request(`${SITE}/api/machines/m_1/open`, {
        method: "POST",
        headers: {
          Cookie: "__Host-al_session=s",
          Origin: SITE,
          "Sec-Fetch-Site": "same-origin",
          "Content-Type": "application/json",
        },
        body: "{}",
      }),
      env(lastSeen),
    );
  const stale = await open(Date.now() - 60 * MINUTE);
  assert.equal(stale.status, 409);
  assert.equal((await stale.json()).error, "offline");

  const page = (lastSeen) =>
    worker.fetch(
      new Request(`${SITE}/m/m_1`, { headers: { Cookie: "__Host-al_session=s" } }),
      env(lastSeen),
    );
  const csp = async (lastSeen) =>
    (await page(lastSeen)).headers.get("Content-Security-Policy");
  assert.match(await csp(Date.now() - MINUTE), /frame-src https:\/\/able-brook-cat\.trycloudflare\.com/);
  assert.doesNotMatch(await csp(Date.now() - 60 * MINUTE), /frame-src/);
});

test("a link approved from another network needs the warning to have been shown", async () => {
  const user = { id: "g_1", email: "me@example.com" };
  const link = {
    device_hash: "d",
    name: "Desk",
    platform: "macos",
    user_code: "ABCD-EFGH",
    ip: "203.0.113.7",
    state: "pending",
    expires_ms: Date.now() + 10 * MINUTE,
  };
  const updates = [];
  const env = fakeEnv({ row: (sql) => (sql.includes("FROM links") ? link : user) });
  const prepare = env.DB.prepare;
  env.DB.prepare = (sql) => {
    const statement = prepare(sql);
    if (sql.startsWith("UPDATE links")) {
      statement.run = async () => {
        updates.push(sql);
        return { meta: { changes: 1 } };
      };
    }
    return statement;
  };
  const ask = (action, from, extra = {}) =>
    worker.fetch(
      new Request(`${SITE}/api/link/${action}`, {
        method: "POST",
        headers: {
          Cookie: "__Host-al_session=s",
          Origin: SITE,
          "Sec-Fetch-Site": "same-origin",
          "Content-Type": "application/json",
          "CF-Connecting-IP": from,
        },
        body: JSON.stringify({ key: "k", ...extra }),
      }),
      env,
    );
  assert.equal((await (await ask("info", "203.0.113.7")).json()).elsewhere, false);
  assert.equal((await (await ask("info", "198.51.100.9")).json()).elsewhere, true);

  const refused = await ask("approve", "198.51.100.9");
  assert.equal(refused.status, 409);
  assert.equal((await refused.json()).error, "confirm_elsewhere");
  assert.equal(updates.length, 0);

  assert.equal((await ask("approve", "198.51.100.9", { elsewhere_ok: true })).status, 200);
  assert.equal((await ask("approve", "203.0.113.7")).status, 200, "same network: as before");
  assert.equal(updates.length, 2);
});
