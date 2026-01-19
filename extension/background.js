const INGEST_URL = "http://127.0.0.1:8787/ingest";
const SCHEMA_VERSION = "1.0";

function safeOriginFromUrl(url) {
  try {
    return new URL(url).hostname || null;
  } catch {
    return null; // e.g. chrome:// pages, about:blank, invalid urls
  }
}

async function sendEvent({ eventType, url, tabId, details = {} }) {
  const payload = {
    schema_version: SCHEMA_VERSION,
    event_type: eventType,
    ts_ms: Date.now(),
    url: url ?? null,
    origin: url ? safeOriginFromUrl(url) : null,
    tab_id: typeof tabId === "number" ? tabId : null,
    details: details ?? {}
  };

  try {
    const res = await fetch(INGEST_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload)
    });

    let body = null;
    try {
      body = await res.json();
    } catch {
      // ignore JSON parse errors
    }

    console.log("Sent event:", payload, "Status:", res.status, "Response:", body);
  } catch (e) {
    console.warn("Failed to send event to desktop daemon:", e);
  }
}

async function handleTabActivated(tabId) {
  try {
    const tab = await chrome.tabs.get(tabId);

    // Many internal pages won't have http(s) URLs and can't be monitored
    if (!tab?.url || !tab.url.startsWith("http")) return;

    await sendEvent({
      eventType: "tab_active",
      url: tab.url,
      tabId: tabId,
      details: {
        title: tab.title ?? null
      }
    });
  } catch (e) {
    console.warn("handleTabActivated error:", e);
  }
}

chrome.tabs.onActivated.addListener(async (activeInfo) => {
  await handleTabActivated(activeInfo.tabId);
});

chrome.tabs.onUpdated.addListener(async (tabId, changeInfo, tab) => {
  try {
    if (changeInfo.status !== "complete") return;
    if (!tab?.active) return;
    if (!tab?.url || !tab.url.startsWith("http")) return;

    await sendEvent({
      eventType: "tab_load_complete",
      url: tab.url,
      tabId,
      details: {
        title: tab.title ?? null
      }
    });
  } catch (e) {
    console.warn("onUpdated error:", e);
  }
});


async function sendRuntimeEvent(message, sender) {
  const tabId = sender?.tab?.id ?? null;
  const url = sender?.tab?.url ?? message.url ?? null;

  await sendEvent({
    eventType: message.event_type,
    url,
    tabId,
    details: message.details ?? {}
  });
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  // Basic validation
  if (!message || typeof message !== "object") return;

  if (message.kind === "hsm_event" && typeof message.event_type === "string") {
    console.log("Received runtime event:", message.event_type, message.details);
    sendRuntimeEvent(message, sender)
      .then(() => sendResponse({ ok: true }))
      .catch((e) => {
        console.warn("Failed to forward event:", e);
        sendResponse({ ok: false, error: String(e) });
      });

    // Required because we respond asynchronously
    return true;
  }
});
