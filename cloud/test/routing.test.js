// Which page each path gets: the landing page at /, the app at /app, and the
// app page again for /link and /m/<id>; signing in returns to the app.
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { safeNext } from "../src/index.js";

const SITE = "https://agentloft.example";
const APP_PATH = "/app";

/** An environment whose static files answer with the path asked for, and
 *  whose database must not be needed. */
function fakeEnv() {
  const asked = [];
  return {
    asked,
    GOOGLE_CLIENT_ID: "client-id",
    GOOGLE_CLIENT_SECRET: "client-secret",
    ASSETS: {
      async fetch(input) {
        const path = new URL(input instanceof Request ? input.url : input).pathname;
        asked.push(path);
        return new Response(`page ${path}`, { headers: { "Content-Type": "text/html" } });
      },
    },
    DB: {
      prepare() {
        throw new Error("the database isn't needed here");
      },
    },
  };
}

async function get(path, env = fakeEnv()) {
  const response = await worker.fetch(new Request(SITE + path, { redirect: "manual" }), env);
  return { response, env };
}

test("/ and /app are static files", async () => {
  for (const path of ["/", "/app", "/landing.js"]) {
    const { response, env } = await get(path);
    assert.equal(response.status, 200);
    assert.deepEqual(env.asked, [path]);
  }
});

test("/link shows the app page under its own policy", async () => {
  const { response, env } = await get("/link");
  assert.equal(response.status, 200);
  assert.deepEqual(env.asked, [APP_PATH]);
  assert.equal(await response.text(), `page ${APP_PATH}`);
  const csp = response.headers.get("Content-Security-Policy");
  assert.match(csp, /script-src 'self'/);
  assert.match(csp, /frame-ancestors 'none'/);
  assert.doesNotMatch(csp, /frame-src/);
  assert.equal(response.headers.get("X-Frame-Options"), "DENY");
});

test("/m/<id> signed out shows the app page, framing nothing", async () => {
  const { response, env } = await get("/m/m_abc");
  assert.equal(response.status, 200);
  assert.deepEqual(env.asked, [APP_PATH]);
  assert.doesNotMatch(response.headers.get("Content-Security-Policy"), /frame-src/);
});

test("signing in starts at Google and comes back to the app", async () => {
  const { response } = await get("/auth/google");
  assert.equal(response.status, 302);
  const location = new URL(response.headers.get("Location"));
  assert.equal(location.origin, "https://accounts.google.com");
  assert.equal(location.searchParams.get("redirect_uri"), `${SITE}/auth/google/callback`);
  assert.match(response.headers.get("Set-Cookie"), new RegExp(`\\.${encodeURIComponent(APP_PATH)};`));

  const linking = await get("/auth/google?next=/link");
  assert.match(linking.response.headers.get("Set-Cookie"), /\.%2Flink;/);
});

test("a failed sign-in returns to the app with the error", async () => {
  const { response } = await get("/auth/google/callback?state=x&code=y");
  assert.equal(response.status, 302);
  const location = response.headers.get("Location");
  assert.ok(location.startsWith(`${APP_PATH}?error=`), location);
});

test("only paths on this site are returned to", () => {
  assert.equal(safeNext("/m/m_abc"), "/m/m_abc");
  assert.equal(safeNext("/link"), "/link");
  assert.equal(safeNext("//evil.example"), APP_PATH);
  assert.equal(safeNext("/\\evil.example"), APP_PATH);
  assert.equal(safeNext("https://evil.example"), APP_PATH);
  assert.equal(safeNext(null), APP_PATH);
});

test("unknown API paths are not pages", async () => {
  const { response, env } = await get("/api/nothing");
  assert.equal(response.status, 404);
  assert.deepEqual(env.asked, []);
});

test("deleting the account needs this site's page, a session and a confirmation", async () => {
  const post = (headers, body) =>
    worker.fetch(
      new Request(SITE + "/api/account/delete", {
        method: "POST",
        headers: { "Content-Type": "application/json", ...headers },
        body: JSON.stringify(body),
      }),
      fakeEnv(),
    );
  // From another site: refused before anything is looked up.
  const cross = await post({ Origin: "https://evil.example" }, { confirm: "delete" });
  assert.equal(cross.status, 403);
  // From this site but signed out: refused (the database is never reached).
  const signedOut = await post({ Origin: SITE, "Sec-Fetch-Site": "same-origin" }, { confirm: "delete" });
  assert.equal(signedOut.status, 401);
});

test("the privacy policy is a static page", async () => {
  const { response, env } = await get("/privacy");
  assert.equal(response.status, 200);
  assert.deepEqual(env.asked, ["/privacy"]);
});

test("Search Console's verification file is answered in place, not redirected", async () => {
  const { response } = await get("/google9485b2468894ad69.html");
  assert.equal(response.status, 200);
  assert.equal(await response.text(), "google-site-verification: google9485b2468894ad69.html");
});
