import { test } from "node:test";
import assert from "node:assert/strict";
import {
  b64url,
  hmac,
  makeTicket,
  randomToken,
  readTicket,
  seal,
  secretsMatch,
  unb64url,
  unseal,
  userCode,
} from "../src/crypto.js";

test("base64url round-trips without padding", () => {
  const bytes = Uint8Array.from([0, 250, 251, 252, 253, 254, 255]);
  const text = b64url(bytes);
  assert.match(text, /^[A-Za-z0-9_-]+$/);
  assert.deepEqual(unb64url(text), bytes);
  assert.throws(() => unb64url("a+b/"));
});

test("random tokens are 256 bits", () => {
  const a = randomToken();
  assert.equal(a.length, 43);
  assert.notEqual(a, randomToken());
});

test("user codes are easy to read", () => {
  for (let i = 0; i < 50; i++) assert.match(userCode(), /^[B-Z2-9]{4}-[B-Z2-9]{4}$/);
});

test("secrets compare", async () => {
  assert.equal(await secretsMatch("abc", "abc"), true);
  assert.equal(await secretsMatch("abc", "abd"), false);
  assert.equal(await secretsMatch("abc", "abcd"), false);
});

// The same vector is checked by the daemon (crates/loftd/src/web/ticket.rs),
// so the two stay in step.
test("HMAC matches RFC 4231 case 2", async () => {
  const mac = await hmac("Jefe", "what do ya want for nothing?");
  assert.equal(
    Buffer.from(unb64url(mac)).toString("hex"),
    "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
  );
});

test("tickets carry who, where and until when", async () => {
  const secret = randomToken();
  const now = Date.UTC(2026, 10, 15, 12);
  const ticket = await makeTicket(secret, {
    machineId: "m_1",
    userId: "g_2",
    email: "a@example.com",
    audience: "https://desk.example.ts.net",
    now,
    ttl: 120,
  });
  const claims = await readTicket(secret, ticket);
  assert.equal(claims.v, 1);
  assert.equal(claims.m, "m_1");
  assert.equal(claims.u, "g_2");
  assert.equal(claims.aud, "https://desk.example.ts.net");
  assert.equal(claims.exp - claims.iat, 120);
  assert.equal(claims.iat, now / 1000);
  assert.equal(await readTicket(randomToken(), ticket), null, "another computer's key");
  const [payload, mac] = ticket.split(".");
  const forged = b64url(new TextEncoder().encode(JSON.stringify({ ...claims, m: "m_9" })));
  assert.equal(await readTicket(secret, `${forged}.${mac}`), null);
  assert.ok(payload);
});

test("sealed secrets open only with the key and their context", async () => {
  const key = randomToken();
  const sealed = await seal(key, "the secret", "m_1");
  assert.equal(await unseal(key, sealed, "m_1"), "the secret");
  await assert.rejects(unseal(key, sealed, "m_2"));
  await assert.rejects(unseal(randomToken(), sealed, "m_1"));
});
