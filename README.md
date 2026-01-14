
# Hybrid Security Monitor

A hybrid **browser + desktop security monitoring system** that connects a **Chrome extension** to a **locally running Rust service**.

The browser captures activity events (tab changes, page loads, etc.) and securely sends them to a **local Rust daemon**, which processes and logs them.
No cloud, no third parties, no tracking — everything runs on your machine.

This project is designed as a **foundation for endpoint security, behavioral monitoring, and zero-trust browser telemetry**.

---

## Architecture Overview

```
[ Chrome Extension ]
          │
          │  HTTP (localhost)
          ▼
[ Rust Desktop Daemon ]
          │
          ▼
[ Logging / Detection Engine ]
```

---

## Features

* Chrome extension captures:

  * Active tab URL
  * Page reloads
  * Timestamped activity
* Rust daemon:

  * Listens on `127.0.0.1:8787`
  * Receives browser telemetry
  * Logs events locally
* Fully local — no internet communication
* Designed to evolve into:

  * Threat detection
  * Browser attack surface monitoring
  * Zero-trust controls

---

## Requirements

You need:

* Google Chrome or Chromium
* Git
* Node.js (LTS)
* Rust (via rustup)

---

## 🔗 Official Install Links

| Tool          | Link                                                             |
| ------------- | ---------------------------------------------------------------- |
| Chrome        | [https://www.google.com/chrome/](https://www.google.com/chrome/) |
| Git           | [https://git-scm.com/install](https://git-scm.com/install)       |
| Node.js (LTS) | [https://nodejs.org/en/download](https://nodejs.org/en/download) |
| Rust (rustup) | [https://rustup.rs/](https://rustup.rs/)                         |

---

## Windows Rust Requirement (VERY IMPORTANT)

On Windows, Rust **requires Microsoft C++ Build Tools**.
If you don’t install them, you will get:

```
error: linker `link.exe` not found
```

Install here:
[https://visualstudio.microsoft.com/downloads/?q=build+tools](https://visualstudio.microsoft.com/downloads/?q=build+tools)

During installation, select:

* **C++ build tools**
* **MSVC v143 (or latest)**
* **Windows 10 or 11 SDK**

After installing, **restart your terminal**.

Verify:

```powershell
where link
where cl
```

Both should print file paths.

---

## Verify All Dependencies

Run:

```bash
git --version
node -v
npm -v
rustc -V
cargo -V
```

---

## Setup

Clone the repository:

```bash
git clone <repository-url>
cd hybrid-security-monitor
```

---

## Running the Desktop Service (Rust)

Start the local Rust daemon:

```bash
cd desktop/daemon
cargo run
```

You should see output showing it is listening on:

```
127.0.0.1:8787
```

Leave this terminal running.

---

## Load the Chrome Extension

1. Open Chrome
2. Go to:

```
chrome://extensions
```

3. Enable **Developer mode**
4. Click **Load unpacked**
5. Select the `extension` folder from this project

The extension is now active.

---

## Verifying It Works

1. Open new tabs
2. Switch between pages
3. Reload pages

In the terminal running the Rust daemon you should see log entries showing:

* URL
* Timestamp
* Activity type

This confirms the browser ↔ desktop bridge is working.

---

## Troubleshooting

### `link.exe not found`

You did not install Visual Studio C++ build tools.
Install from:
[https://visualstudio.microsoft.com/downloads/?q=build+tools](https://visualstudio.microsoft.com/downloads/?q=build+tools)

Then reopen PowerShell and retry:

```bash
cargo build
```

---

### `curl -sSf` fails on Windows

PowerShell’s `curl` is not real curl.
Install Rust via:
[https://rustup.rs/](https://rustup.rs/)

---

### Port already in use

If port `8787` is busy, close any other running daemon or change the port in the Rust server.

---

## Current State

This is **Phase 1** of the platform:

* Transport
* Telemetry
* Local service

No detection, blocking, or intelligence has been added yet.

Those come next.

---

## Security Model

* All data stays on localhost
* No cloud
* No external APIs
* No telemetry leaves the machine

This is the foundation of a **zero-trust endpoint security agent**.
