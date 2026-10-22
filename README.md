# Agent Deck Companion

[English](README.md) · [简体中文](README.zh-CN.md)

**A local-first desktop panel that keeps every AI coding agent on your machine in view — no hardware required.**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey.svg)](https://github.com/xuan-team/agent-deck-companion)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB.svg)](https://tauri.app)
[![Back us on Kickstarter](https://img.shields.io/badge/Kickstarter-back%20us-05ce78?logo=kickstarter&logoColor=white)](https://www.kickstarter.com/projects/agentdeck/agentdeck-the-physical-control-deck-for-ai-coding-agents)

When you run several agents at once, it is hard to tell which session is working, which one is
blocked waiting for you, and which one just produced a new result. Companion watches them from one
small panel and drops you back into the exact session with a click. It is a control surface for your
agents — not an IDE.

> **Status:** macOS only today. Windows and Linux are planned, but not yet supported or validated.
> Built with Tauri 2, Rust, SvelteKit and TypeScript.

---

## Demo

[![AgentDeck demo: from manual AI agent switching to one-press control](https://img.youtube.com/vi/Uf_wwHSCPIs/maxresdefault.jpg)](https://youtu.be/Uf_wwHSCPIs)

▶️ **Watch on YouTube** — a walkthrough of Agent Deck Companion on its own, and the optional
AgentDeck hardware on top of it.

---

## Why

Modern development spreads across many agents, terminals, IDE windows and browser tabs. Every
`Alt-Tab` is a guess about which one needs you. Companion makes that state visible in one place:

- **See** what each session is doing right now (working, needs input, finished, errored).
- **Focus** on the sessions that actually want you, without stealing focus from your work.
- **Return** to the exact session — the right terminal, or the right ChatGPT thread.

## Features

- **Session panel** — a resident panel lists sessions across every monitored agent. Click a row to
  open that session. The standalone panel supports project grouping and three reorderable views:
  **Needs action**, **Codex**, and **Other agents**.
- **Attention queue** — sessions that need input, errored, or produced a new result are collected in
  one queue with a read/unread state.
- **Notifications** — in-panel alert cards while the panel is visible, and optional macOS desktop
  notifications when it is hidden. Notifications never grab focus; clicking one returns to the
  session or the attention queue.
- **Jump to session** — uses whatever locator the session exposes (terminal, pane, or — for
  Codex / ChatGPT — a `codex://threads/<id>` deep link). When a session cannot be located, Companion
  says so instead of guessing.
- **Lightweight floating panel** — collapse, hide, reorder tabs, and bring it back from the menu bar.
  Colors follow the shared theme selected in the main window.
- **Per-agent monitoring** — enable only the agents you actually use in **Settings → Agent
  Monitoring**. Disabled agents stop being detected and disappear from the list (slot bindings are
  preserved and restored when re-enabled).

## Works without hardware

**You do not need any AgentDeck hardware to use Companion.** Hardware listening is off by default,
and the whole session-management experience above is available on its own.

AgentDeck is an optional ESP32-based companion keyboard that adds physical keys, a screen, an encoder
knob and notification sounds on top of the same desktop capabilities. See
[Optional: AgentDeck hardware](#optional-agentdeck-hardware).

## Supported agents

Session sources differ in the granularity of their status and how precisely they can be re-focused.
Companion detects processes and state through per-agent adapters; process presence does not guarantee
that every individual session can be identified.

| Agent | Detection | Lifecycle status |
| --- | --- | --- |
| ChatGPT Desktop / Codex | Native | Process + thread list, deep-link jump |
| Claude Code | Process + hooks | Via installed hooks |
| Qwen Code | Process + hooks | Via installed hooks |
| Codex CLI | Process + hooks | Via installed hooks |
| Herdr (CLI agents) | Native integration | Per-pane |
| DeepSeek Harness (DSH) | Native integration + `dsh-deck-bridge` plugin | Per-session |
| OpenCode / Qoder | Plugin | Per-session |

CLI agents report lifecycle state through hooks that you install explicitly. DeepSeek Harness uses
the bundled `dsh-deck-bridge` plugin. See [`doc/`](doc/) for per-agent notes.

## Getting started

### Prerequisites

- macOS (Apple Silicon validated; Intel needs a separate build). Windows and Linux are planned.
- Node.js 20+ and npm
- Rust stable
- [Tauri 2 system dependencies](https://tauri.app/start/prerequisites/) for your platform

### Run from source

```bash
npm install
npm run tauri:dev
```

1. Start Companion, then start the agents you use.
2. The default **Sessions** view lists discovered sessions. Click **Open session panel**, or show/hide
   it from the menu bar.
3. For CLI lifecycle status, open **Hooks** and install hooks only for the agents you use. The first
   time Codex CLI loads new hooks, review and trust them in the CLI's `/hooks`.
4. Enable system notifications in the **Notifications** page if you want them. Desktop notifications
   require the packaged macOS app; the development window may not deliver them.
5. In **Settings → Agent Monitoring**, enable only the agents you use. Disabled agents are no longer
   detected and their sessions leave the list.
6. Click a session to return to its window. If Companion cannot locate it, check that the agent is
   still running, and your terminal and permissions setup.

Regular users do not need to enable ESP32 hardware support or configure A1–A5 / K1–K10 keys.

### Build the macOS app

```bash
npm run tauri:build -- --bundles app
```

The bundle is written to `src-tauri/target/release/bundle/macos/`. Builds are ad-hoc signed, so you
may need to re-grant permissions after rebuilding. See the
[macOS release guide](doc/macos_release.md).

Tagged releases (`v*`) are built by GitHub Actions into a universal DMG and published on the
[releases page](https://github.com/xuan-team/agent-deck-companion/releases). Those builds are ad-hoc
signed and not notarized, so run this once after installing:

```bash
xattr -dr com.apple.quarantine "/Applications/Agent Deck Companion.app"
```

## Optional: AgentDeck hardware

AgentDeck is an ESP32-based companion keyboard that adds physical keys, a screen, a knob and
keyboard notification sounds to the same desktop features. Companion's standalone software features
are not locked behind hardware.

Only hardware users need to open the **AgentDeck Hardware (optional)** page:

1. Enable **ESP32 hardware support** from the menu bar, then connect the device. Keyboard HID works
   over USB and BLE; screen state sync (Vendor HID) is currently BLE-only.
2. Grant Input Monitoring when asked; sending simulated shortcuts may also need Accessibility.
   Restart the app after granting Input Monitoring.
3. Click **Configure keys** to bind **A1–A5** as agent selectors. A slot can be bound to **Companion**
   for quick session-panel navigation.
4. In Companion navigation, use K1–K3 / K6–K8 to open sessions on the current page, K4/K5 to page,
   K9 to exit navigation and K10 to show/hide the panel; the knob scrolls views or pages. Physical
   navigation depends on the firmware version; the UI points out anything still awaiting firmware
   support.
5. Configure **K1–K10** agent actions, function-key hints and keyboard notification sounds as needed.

New configurations default to A1=Qwen, A2=Herdr, A3=ChatGPT, A4=Claude, A5=Companion. Existing
configurations are preserved. Codex CLI and ChatGPT Desktop currently share one optional agent slot.

With hardware support off, restart the app to fully stop any listeners that were started. With no
device attached you can still simulate key presses from the hardware page for development.

Key and protocol details: [`key_map.md`](key_map.md), the
[notification audio protocol](notification_audio_protocol.md), and [hardware troubleshooting](doc/qa.md).

## Privacy and local data

- **Basic status viewing** does not require Input Monitoring or Accessibility. Some window targeting
  and shortcut sending may need Accessibility/Automation; Input Monitoring is used for global
  hardware keys.
- **System notifications** are opt-in and subject to your macOS notification and Do Not Disturb
  settings.
- **Hooks** are installed and removed explicitly and merge with existing config. Before the first
  change to an existing config, a `*.agent-deck-backup` copy is created. Uninstall removes only
  handlers tagged by Companion.
- The hook bridge extracts only metadata — session/task identifiers, working directory, event name
  and timestamps. It never stores prompt, tool input, tool response or transcript content.
- The local API listens only on `127.0.0.1:34115` and hooks authenticate with a local token. Do not
  expose this local debugging API to a public network.
- Configuration stays on your machine. On macOS it lives in
  `~/Library/Application Support/Agent Deck Companion/`; the hooks bridge lives in
  `~/.agent-deck-companion/`.

## Architecture and development

- `src/` — SvelteKit UI and the TypeScript API client.
- `src-tauri/` — Rust state, agent adapters, windows, process and device integration.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — how the pieces fit together.
- [`AGENTS.md`](AGENTS.md) — architecture boundaries and verification expectations for contributors.

Run the checks relevant to the area you touched:

```bash
npm run check
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
```

For event-path changes, also exercise `/api/hardware/key` and `/api/mock/status` while the Tauri app
is running. Running `npm run dev` on its own still needs an existing Tauri backend for the local API,
so it is not a replacement for the desktop app.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full workflow, and [SECURITY.md](SECURITY.md) to
report a vulnerability. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Support the project

Agent Deck Companion is free and open source under MIT. The **AgentDeck hardware** is crowdfunding on
Kickstarter — backing the campaign, or starring this repository, helps us keep building.

- 💛 [Back AgentDeck on Kickstarter](https://www.kickstarter.com/projects/agentdeck/agentdeck-the-physical-control-deck-for-ai-coding-agents)
- ▶️ [Watch the demo on YouTube](https://youtu.be/Uf_wwHSCPIs)

## License

[MIT](LICENSE) © 2026 xuan team
