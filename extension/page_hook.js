(() => {
  if (window.__HSM_PAGE_HOOKED__) return;
  window.__HSM_PAGE_HOOKED__ = true;

  function emit(eventType, details = {}) {
    window.postMessage(
      { source: "hsm_page", event_type: eventType, details },
      "*"
    );
  }

  // Worker hook
  try {
    const OriginalWorker = window.Worker;
    if (typeof OriginalWorker === "function") {
      window.Worker = function (...args) {
        const scriptUrl = args?.[0] ? String(args[0]) : null;
        emit("worker_created", { worker_type: "Worker", script_url: scriptUrl });
        return new OriginalWorker(...args);
      };
      window.Worker.prototype = OriginalWorker.prototype;
    }
  } catch {}

  // SharedWorker hook
  try {
    const OriginalSharedWorker = window.SharedWorker;
    if (typeof OriginalSharedWorker === "function") {
      window.SharedWorker = function (...args) {
        const scriptUrl = args?.[0] ? String(args[0]) : null;
        emit("worker_created", { worker_type: "SharedWorker", script_url: scriptUrl });
        return new OriginalSharedWorker(...args);
      };
      window.SharedWorker.prototype = OriginalSharedWorker.prototype;
    }
  } catch {}

  // WASM hooks
  try {
    const WA = window.WebAssembly;
    if (WA && typeof WA.instantiate === "function") {
      const originalInstantiate = WA.instantiate.bind(WA);
      WA.instantiate = async function (...args) {
        emit("wasm_instantiate", {
          method: "instantiate",
          arg0_type: args?.[0] ? typeof args[0] : null
        });
        return originalInstantiate(...args);
      };
    }

    if (WA && typeof WA.instantiateStreaming === "function") {
      const originalInstantiateStreaming = WA.instantiateStreaming.bind(WA);
      WA.instantiateStreaming = async function (...args) {
        emit("wasm_instantiate", { method: "instantiateStreaming" });
        return originalInstantiateStreaming(...args);
      };
    }
  } catch {}

  // Tab visibility tracking
  try {
    const emitVisibility = () => {
      const state = document.visibilityState === "hidden" ? "hidden" : "visible";
      emit("tab_visibility", { state });
    };

    document.addEventListener("visibilitychange", emitVisibility);
    emitVisibility();
  } catch {}

  // User activity heartbeat (rate-limited)
  try {
    const ACTIVITY_RATE_MS = 5000;
    let lastActivityMs = 0;

    const emitActivity = () => {
      const now = Date.now();
      if (now - lastActivityMs < ACTIVITY_RATE_MS) return;
      lastActivityMs = now;
      emit("user_activity", { active: true });
    };

    ["mousemove", "keydown", "scroll", "click"].forEach((eventName) => {
      window.addEventListener(eventName, emitActivity, { passive: true });
    });
  } catch {}

  // Media playback signal (no content)
  try {
    const MEDIA_HEARTBEAT_MS = 10000;
    let lastMediaEmitMs = 0;

    const emitMediaState = (state) => {
      const now = Date.now();
      if (state === "playing" && now - lastMediaEmitMs < MEDIA_HEARTBEAT_MS) return;
      lastMediaEmitMs = now;
      emit("media_playing", { state });
    };

    document.addEventListener(
      "play",
      () => emitMediaState("playing"),
      { capture: true, passive: true }
    );
    document.addEventListener(
      "pause",
      () => emitMediaState("paused"),
      { capture: true, passive: true }
    );
    document.addEventListener(
      "ended",
      () => emitMediaState("paused"),
      { capture: true, passive: true }
    );
    document.addEventListener(
      "timeupdate",
      (event) => {
        const target = event.target;
        if (target && typeof target.paused === "boolean" && !target.paused) {
          emitMediaState("playing");
        }
      },
      { capture: true, passive: true }
    );
  } catch {}

  emit("instrumentation_loaded", { location: location.href });
})();
