async function sendEvent(url) {
  const payload = {
    event_type: "tab_active",
    url,
    ts_ms: Date.now()
  };

  try {
    const res = await fetch("http://127.0.0.1:8787/ingest", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload)
    });

    // Optional: log in extension service worker console
    console.log("Sent event:", payload, "Status:", res.status);
  } catch (e) {
    console.warn("Failed to send event to desktop daemon:", e);
  }
}

chrome.tabs.onActivated.addListener(async (activeInfo) => {
  const tab = await chrome.tabs.get(activeInfo.tabId);
  if (tab?.url) await sendEvent(tab.url);
});

chrome.tabs.onUpdated.addListener(async (tabId, changeInfo, tab) => {
  if (changeInfo.status === "complete" && tab.active && tab.url) {
    await sendEvent(tab.url);
  }
});
