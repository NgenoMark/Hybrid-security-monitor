# Test Pages

Purpose-built pages to trigger predictable signals for the extension + daemon.

## Pages

- `worker_only.html`: Spawns a low-CPU Web Worker.
- `wasm_only.html`: Instantiates a tiny WASM module.
- `cpu_burn.html`: Runs a sustained CPU loop (main thread or worker).

## Run locally

```powershell
cd docs/test_pages
.\serve.ps1
```

Then open:

- http://127.0.0.1:8000/worker_only.html
- http://127.0.0.1:8000/wasm_only.html
- http://127.0.0.1:8000/cpu_burn.html

Keep the daemon running and watch the dashboard/terminal output.
