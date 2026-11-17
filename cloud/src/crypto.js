// Small cryptographic helpers on WebCrypto. Shared by the Worker and its
// tests (Node has the same `crypto.subtle`).

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Bytes as unpadded base64url. */
export function b64url(bytes) {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

/** Unpadded (or padded) base64url as bytes; throws on anything else. */
export function unb64url(text) {
  if (!/^[A-Za-z0-9_-]*={0,2}$/.test(text)) throw new Error("not base64url");
  const base64 = text.replaceAll("-", "+").replaceAll("_", "/");
  const binary = atob(base64 + "=".repeat((4 - (base64.length % 4)) % 4));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}

/** `n` random bytes from the OS, as base64url. 32 bytes: 43 characters. */
export function randomToken(n = 32) {
  return b64url(crypto.getRandomValues(new Uint8Array(n)));
}

/** A code people read and compare: 8 letters and digits that can't be
 *  mistaken for one another, as `ABCD-EFGH`. */
export function userCode() {
  const alphabet = "BCDFGHJKLMNPQRSTVWXZ23456789";
  const bytes = crypto.getRandomValues(new Uint8Array(8));
  let code = "";
  for (const [i, byte] of bytes.entries()) {
    if (i === 4) code += "-";
    code += alphabet[byte % alphabet.length];
  }
  return code;
}

export async function sha256(text) {
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function sha256b64url(text) {
  return b64url(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(text))));
}

/** Whether two strings are equal, in time that doesn't depend on where
 *  they differ (both are hashed first, so length doesn't leak either). */
export async function secretsMatch(a, b) {
  const [x, y] = await Promise.all([sha256(a), sha256(b)]);
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x.charCodeAt(i) ^ y.charCodeAt(i);
  return diff === 0;
}

/** HMAC-SHA256 of `message` keyed with the UTF-8 bytes of `secret`. */
export async function hmac(secret, message) {
  const key = await crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return b64url(new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(message))));
}

/**
 * A ticket: a short-lived note that lets one browser sign in to one
 * computer, signed with that computer's own key. Agentloft's daemon checks
 * it (`crates/loftd/src/web/ticket.rs`):
 *
 *   base64url(JSON payload) "." base64url(HMAC-SHA256(secret, first part))
 *
 * Payload: `v` (1), `m` machine id, `u` account id, `e` email, `aud` the
 * origin it is for, `iat`/`exp` (seconds), `n` a nonce it is used once by.
 */
export async function makeTicket(secret, { machineId, userId, email, audience, now, ttl }) {
  const iat = Math.floor(now / 1000);
  const payload = b64url(
    encoder.encode(
      JSON.stringify({
        v: 1,
        m: machineId,
        u: userId,
        e: email,
        aud: audience,
        iat,
        exp: iat + ttl,
        n: randomToken(16),
      }),
    ),
  );
  return `${payload}.${await hmac(secret, payload)}`;
}

/** The payload of a ticket made with `secret`, or null. (The daemon has its
 *  own copy of this; this one is for tests.) */
export async function readTicket(secret, ticket) {
  const [payload, mac] = ticket.split(".");
  if (!payload || !mac) return null;
  if (!(await secretsMatch(mac, await hmac(secret, payload)))) return null;
  return JSON.parse(decoder.decode(unb64url(payload)));
}

async function sealingKey(keyText) {
  const raw = unb64url(keyText);
  if (raw.length !== 32) throw new Error("SECRETS_KEY must be 32 bytes, base64url");
  return crypto.subtle.importKey("raw", raw, "AES-GCM", false, ["encrypt", "decrypt"]);
}

/** `text` sealed with AES-256-GCM under `keyText` (base64url, 32 bytes),
 *  bound to `context` (what it belongs to, so a sealed value can't be moved
 *  to another row). */
export async function seal(keyText, text, context) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const sealed = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv, additionalData: encoder.encode(context) },
    await sealingKey(keyText),
    encoder.encode(text),
  );
  return `${b64url(iv)}.${b64url(new Uint8Array(sealed))}`;
}

export async function unseal(keyText, sealedText, context) {
  const [iv, data] = sealedText.split(".");
  const plain = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: unb64url(iv), additionalData: encoder.encode(context) },
    await sealingKey(keyText),
    unb64url(data),
  );
  return decoder.decode(plain);
}

/** The claims of a JWT, unchecked: only for an ID token received straight
 *  from Google's token endpoint over TLS, which needs no signature check. */
export function jwtClaims(token) {
  const part = String(token).split(".")[1];
  if (!part) throw new Error("not a JWT");
  return JSON.parse(decoder.decode(unb64url(part)));
}
