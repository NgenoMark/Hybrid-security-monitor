(() => {
  // Bridge page -> content script -> background
  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    const msg = event.data;
    if (!msg || msg.source !== "hsm_page") return;

    try {
      chrome.runtime.sendMessage({
        kind: "hsm_event",
        event_type: msg.event_type,
        details: msg.details ?? {}
      });
    } catch {}
  });
})();
