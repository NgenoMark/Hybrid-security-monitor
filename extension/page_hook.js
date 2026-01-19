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

  emit("instrumentation_loaded", { location: location.href });
})();
