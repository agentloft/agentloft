// Loads the wasm app. Kept as a file (no inline script) so the page works
// under a strict Content-Security-Policy.
import init from "./loft_web.js";

const boot = document.getElementById("boot");
try {
  await init({ module_or_path: new URL("./loft_web_bg.wasm", import.meta.url) });
  boot?.remove();
} catch (err) {
  console.error(err);
  if (boot) boot.textContent = "Agentloft could not start in this browser.";
}
