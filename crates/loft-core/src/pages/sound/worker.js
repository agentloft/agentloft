// Agentloft's browser plays sound on the speakers of the computer it runs
// on. Every tab is muted except those the daemon names with `hear`: tabs
// an Agentloft window on this computer is showing (pages/sound.rs).
// Chrome's own tab muting, so nothing a page does gets around it.

async function apply() {
  const { loud = [] } = await chrome.storage.session.get("loud");
  const keep = new Set(loud);
  // The daemon knows tabs by DevTools target id; `chrome.tabs` by tab id.
  for (const target of await chrome.debugger.getTargets()) {
    if (target.tabId === undefined) continue;
    const muted = !keep.has(target.id);
    const tab = await chrome.tabs.get(target.tabId).catch(() => null);
    if (tab && tab.mutedInfo.muted !== muted) {
      await chrome.tabs.update(tab.id, { muted }).catch(() => {});
    }
  }
}

// The daemon's call: these targets may be heard, no others.
self.hear = async (loud) => {
  await chrome.storage.session.set({ loud });
  await apply();
  return true;
};

// Whether a target's tab is muted (for the daemon's tests).
self.muted = async (id) => {
  const target = (await chrome.debugger.getTargets()).find((t) => t.id === id);
  if (!target || target.tabId === undefined) return null;
  return (await chrome.tabs.get(target.tabId)).mutedInfo.muted;
};

// A new tab is quiet until it is named.
chrome.tabs.onCreated.addListener((tab) => {
  chrome.tabs.update(tab.id, { muted: true }).catch(() => {}).then(apply);
});

apply();
