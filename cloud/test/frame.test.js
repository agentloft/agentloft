// What the frame on /m/<id> lets a computer's page do. The page inside
// comes from that computer, so it gets only what Agentloft there needs.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const page = await readFile(new URL("../public/app.html", import.meta.url), "utf8");
const script = await readFile(new URL("../public/app.js", import.meta.url), "utf8");

/** The `allow` list of the frame a computer is shown in. */
function frameAllows() {
  const frames = page.match(/<iframe\b[^>]*>/g) ?? [];
  assert.equal(frames.length, 1, "one frame on the page");
  const allow = /\sallow="([^"]*)"/.exec(frames[0]);
  assert.ok(allow, "the frame says what it allows");
  return allow[1]
    .split(";")
    .map((feature) => feature.trim())
    .filter(Boolean);
}

test("a framed computer can't read the clipboard, only write it", () => {
  const allows = frameAllows();
  // Pasting is the browser's own paste event, which needs no permission:
  // a page that can read the clipboard could read it whenever it likes.
  assert.ok(!allows.some((feature) => feature.startsWith("clipboard-read")), allows.join("; "));
  assert.ok(allows.includes("clipboard-write"), "copying from Agentloft still works");
  // Nor does the script hand it out later.
  assert.doesNotMatch(script, /clipboard-read/);
  assert.doesNotMatch(script, /\.allow\s*=|setAttribute\(\s*["']allow["']/);
});
