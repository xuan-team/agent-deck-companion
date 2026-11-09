# Windows notes

Windows 10/11 is supported as a preview platform. The companion, the session
panel, process detection, hook integrations and window focusing all build and
run natively; several macOS-only integrations have no Windows backend yet.

## Build and run

```powershell
npm install
npm run tauri:dev
npm run tauri:build
```

`npm run tauri:build` produces an NSIS installer and an MSI under
`src-tauri/target/release/bundle/`. The Windows icon set (`icon.ico` and the
`Square*Logo.png` family) is generated from `src-tauri/icons/icon.png` with
`npx tauri icon src-tauri/icons/icon.png --output src-tauri/icons`.

## What works

- **Process detection.** `sysinfo` process names and command lines are matched
  case-insensitively; a `.exe` suffix is ignored on both sides, and Windows
  backslash paths are compared against the POSIX-style patterns in the plugin
  manifests.
- **Hooks.** Installing a hook writes a PowerShell bridge to
  `%USERPROFILE%\.agent-deck-companion\bin\agent-deck-hook.ps1` and registers a
  `powershell.exe -File ...` command in the agent config. The bridge forwards
  the hook payload, the token and the agent parent PID to the loopback API.
- **Session panel.** Clicking a row raises the owning top-level window through
  Win32 (`win_focus.rs`); DSH rows open the web profile and Codex/ChatGPT
  threads use the `codex://threads/<id>` protocol handler.
- **Agent switching.** Clicking an agent row, or pressing its A-key with
  hardware enabled, focuses the matching window (process ancestry, terminal
  title and the project folder name are used as evidence; a single terminal
  window is used as a last-resort fallback).
- **Mock input and the loopback API** (`/health`, `/api/state`,
  `/api/hardware/key`, `/api/mock/status`, …) behave the same as on macOS.
- **Vendor HID** uses `hidapi` and is compiled on Windows.

## Known gaps

- **Herdr and tmux** are Unix-socket integrations. Herdr snapshots stay empty
  and Herdr/tmux session rows cannot be opened on Windows.
- **Keyboard HID** (the `CGEventTap` listener that turns the deck's A/K keys
  into input) is macOS-only. Vendor HID and the mock HTTP input still work.
- **Desktop notifications** use the macOS notification adapter; on Windows the
  in-panel attention queue and cards remain available, but the OS notification
  channel is reported as unavailable.
- **Session activity cache** shells out to the `sqlite3` CLI. When it is not on
  `PATH` the cache is skipped (with a diagnostic log); the rest of the app is
  unaffected. Install the SQLite tools if you want activity history to persist.
- **ChatGPT encoder sync** (reasoning-level changes caused by the knob) is
  macOS-only.
- **Hooks inside Herdr/tmux panes** depend on the Unix pane environment
  variables and do not apply on Windows.
