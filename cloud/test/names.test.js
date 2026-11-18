// A computer's name: the one it asked to be linked with, as approved on
// the website; after that only its owner renames it here. Its reports
// can't: a computer (or whoever has its key) must not relabel itself in
// someone's list.
import { test } from "node:test";
import assert from "node:assert/strict";
import worker from "../src/index.js";
import { randomToken, seal } from "../src/crypto.js";

const SITE = "https://agentloft.example";
const SECRETS_KEY = randomToken();
const KEY = randomToken();
const USER = { id: "g_1", email: "me@example.com" };

/** One computer, `m_1`, of `g_1`'s, and a database that keeps what is run
 *  on it: `writes` holds each `run()` with its SQL and bound values. */
async function fakeEnv({ owner = "g_1" } = {}) {
  const machine = {
    id: "m_1",
    user_id: owner,
    name: "Desk",
    platform: "macos",
    version: "1.0.0",
    url: null,
    public: 0,
    secret: await seal(SECRETS_KEY, KEY, "m_1"),
    email: "me@example.com",
    created_ms: Date.now(),
    last_seen_ms: Date.now(),
  };
  const writes = [];
  const env = {
    writes,
    SECRETS_KEY,
    ALLOWED_EMAILS: "",
    ASSETS: { fetch: async () => new Response("page") },
    DB: {
      prepare(sql) {
        let values = [];
        const statement = {
          bind: (...args) => {
            values = args;
            return statement;
          },
          first: async () => {
            if (sql.includes("FROM sessions")) return USER;
            if (sql.includes("FROM machines WHERE id = ?1 AND user_id = ?2")) {
              return values[1] === machine.user_id ? machine : null;
            }
            if (sql.includes("FROM machines")) return machine;
            return null;
          },
          all: async () => ({ results: [machine] }),
          run: async () => {
            writes.push({ sql, values });
            return { meta: { changes: 1 } };
          },
        };
        return statement;
      },
      batch: async () => [],
    },
  };
  return env;
}

function report(env, body) {
  return worker.fetch(
    new Request(`${SITE}/api/machine/report`, {
      method: "POST",
      headers: { Authorization: `Bearer m_1.${KEY}`, "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
    env,
  );
}

function rename(env, body, headers = {}) {
  return worker.fetch(
    new Request(`${SITE}/api/machines/m_1/rename`, {
      method: "POST",
      headers: {
        Cookie: "__Host-al_session=s",
        Origin: SITE,
        "Sec-Fetch-Site": "same-origin",
        "Content-Type": "application/json",
        ...headers,
      },
      body: typeof body === "string" ? body : JSON.stringify(body),
    }),
    env,
  );
}

test("a computer's reports don't change its name", async () => {
  const env = await fakeEnv();
  const reply = await report(env, {
    name: "Your bank: sign in again",
    url: "https://able-brook-cat.trycloudflare.com",
    public: true,
    platform: "macos",
    version: "1.2.0",
  });
  assert.equal(reply.status, 200);
  assert.equal(env.writes.length, 1);
  const [{ sql, values }] = env.writes;
  assert.match(sql, /^\s*UPDATE machines SET/);
  assert.doesNotMatch(sql, /\bname\b/, "the report sets no name");
  assert.ok(!values.includes("Your bank: sign in again"), "nor is the name it sent bound");
  // What a report is for still lands.
  assert.ok(values.includes("https://able-brook-cat.trycloudflare.com"));
  assert.ok(values.includes("1.2.0"));
});

test("the owner renames a computer", async () => {
  const env = await fakeEnv();
  const reply = await rename(env, { name: "  Studio\u0007 Mac\n " });
  assert.equal(reply.status, 200);
  assert.deepEqual(await reply.json(), { ok: true, name: "Studio Mac" });
  assert.equal(env.writes.length, 1);
  const [{ sql, values }] = env.writes;
  assert.match(sql, /^UPDATE machines SET name = \?1 WHERE id = \?2 AND user_id = \?3$/);
  assert.deepEqual(values, ["Studio Mac", "m_1", "g_1"]);

  // Long names are cut to the length a linking computer's is.
  const long = await fakeEnv();
  assert.equal((await rename(long, { name: "x".repeat(500) })).status, 200);
  assert.equal(long.writes[0].values[0], "x".repeat(80));
});

test("a rename needs a name, this site's page, and a computer of one's own", async () => {
  for (const name of ["", "   ", "\u0001", 42, null]) {
    const env = await fakeEnv();
    const reply = await rename(env, { name });
    assert.equal(reply.status, 400, JSON.stringify(name));
    assert.equal(env.writes.length, 0);
  }

  const crossSite = await fakeEnv();
  const elsewhere = await rename(crossSite, { name: "Mine now" }, {
    Origin: "https://evil.example",
    "Sec-Fetch-Site": "cross-site",
  });
  assert.equal(elsewhere.status, 403);
  assert.equal(crossSite.writes.length, 0);

  const notJson = await fakeEnv();
  const form = await rename(notJson, "name=Mine", {
    "Content-Type": "application/x-www-form-urlencoded",
  });
  assert.equal(form.status, 415);
  assert.equal(notJson.writes.length, 0);

  const someoneElses = await fakeEnv({ owner: "g_2" });
  const theirs = await rename(someoneElses, { name: "Mine now" });
  assert.equal(theirs.status, 404);
  assert.equal(someoneElses.writes.length, 0);

  const signedOut = await fakeEnv();
  const anon = await worker.fetch(
    new Request(`${SITE}/api/machines/m_1/rename`, {
      method: "POST",
      headers: { Origin: SITE, "Sec-Fetch-Site": "same-origin", "Content-Type": "application/json" },
      body: JSON.stringify({ name: "Mine now" }),
    }),
    signedOut,
  );
  assert.equal(anon.status, 401);
  assert.equal(signedOut.writes.length, 0);
});
