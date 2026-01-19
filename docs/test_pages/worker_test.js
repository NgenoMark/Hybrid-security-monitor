const w = new Worker("./worker_test_worker.js");
w.onmessage = (e) => console.log("Worker:", e.data);
w.postMessage("ping");
