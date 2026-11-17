// The landing page (/): offer the download for this visitor's system first,
// and send someone already signed in to their computers rather than to
// "Sign in". A file, not an inline script, so the page runs under a strict
// CSP. Without it the page still works: macOS first, "Sign in".

/** "mac", "windows", "linux", "mobile" or null. */
function system() {
  const ua = navigator.userAgent;
  const platform = navigator.userAgentData?.platform || navigator.platform || "";
  if (/iPhone|iPad|iPod|Android/i.test(ua) || navigator.userAgentData?.mobile) return "mobile";
  // iPadOS asks for desktop pages as a Mac, but has a touch screen.
  if (/Mac/i.test(platform) && navigator.maxTouchPoints > 1) return "mobile";
  if (/Mac/i.test(platform)) return "mac";
  if (/Win/i.test(platform) || /Windows NT/.test(ua)) return "windows";
  if (/Linux|X11|CrOS/i.test(platform) || /Linux/i.test(ua)) return "linux";
  return null;
}

const LABELS = { mac: "macOS", windows: "Windows", linux: "Linux" };

function offer(os) {
  for (const group of document.querySelectorAll(".downloads")) {
    const button = group.querySelector(`[data-os="${os}"]`);
    if (!button) continue;
    for (const other of group.querySelectorAll("[data-os]")) {
      const primary = other === button;
      other.classList.toggle("btn-primary", primary);
      other.querySelector("span").textContent = primary
        ? `Download for ${LABELS[other.dataset.os]}`
        : LABELS[other.dataset.os];
    }
    group.prepend(button);
  }
  for (const note of document.querySelectorAll("[data-note]")) {
    note.hidden = note.dataset.note !== os;
  }
}

const os = system();
if (os === "mobile") {
  document.querySelector(".phone-note").hidden = false;
} else if (os && os !== "mac") {
  offer(os);
}

// Signed in already: the way in is "Your computers".
try {
  const reply = await fetch("/api/me", { cache: "no-store", credentials: "same-origin" });
  if (reply.ok) {
    const account = document.getElementById("account");
    account.textContent = "Your computers";
    account.href = "/app";
  }
} catch {}
