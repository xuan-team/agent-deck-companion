# Agent Deck Companion — Architecture

> Local-first desktop companion for AI coding agents. One panel tracks every agent session and jumps
> back to the exact session. AgentDeck hardware (an ESP32 deck with A-keys, K-keys and an encoder
> knob) is an optional accelerator on top of the same capabilities.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Frontend | SvelteKit 2 + Svelte 5 + TypeScript + Vite 7 |
| Shell | Tauri 2 (macOS-first) |
| Backend / Core | Rust (axum, hidapi, sysinfo, core-graphics) |
| Hardware | ESP32 via USB HID (Vendor Report ID 6) + Keyboard HID (F13–F17) |
| IPC | Tauri commands + events · HTTP API on `127.0.0.1:34115` |

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        Tauri Shell                               │
│                                                                  │
│  ┌─────────────────┐   Tauri Commands    ┌────────────────────┐  │
│  │   SvelteKit UI   │ ◄────────────────► │    Rust Core        │  │
│  │   (Webview)      │   Events           │    (lib.rs)         │  │
│  └─────────────────┘                     └────────┬───────────┘  │
│                                                   │              │
│  ┌─────────────────┐   HTTP (loopback)   ┌───────┴───────────┐  │
│  │  External Agents  │ ◄───────────────► │   axum HTTP API    │  │
│  │  (hooks / mocks)  │   port 34115      │   (http_api.rs)    │  │
│  └─────────────────┘                     └───────────────────┘  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │              Hardware Layer (macOS only)                  │    │
│  │  hardware_hid.rs (keyboard F13-F17)  │  vendor_hid.rs    │    │
│  │  → CGEventTap for A/K keys           │  → HID Report ID 6│    │
│  └──────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────┘
```

---

## Directory Structure

```
agent_deck_companion/
├── src/                          # SvelteKit frontend
│   ├── routes/
│   │   ├── +page.svelte          # Main UI (single-page app)
│   │   └── +layout.ts            # Static layout
│   └── lib/
│       ├── api.ts                # Tauri invoke + HTTP fallback wrappers
│       └── types.ts              # TypeScript type definitions (mirrors Rust)
│
├── src-tauri/
│   ├── src/
│   │   ├── main.rs               # Entry point → calls lib::run()
│   │   ├── lib.rs                # App bootstrap, Tauri setup, process_event()
│   │   ├── state.rs              # Core state model (AppRuntime, AppSnapshot, …)
│   │   ├── commands.rs           # Tauri command handlers (invoke from UI)
│   │   ├── http_api.rs           # axum HTTP server (mock + hook endpoints)
│   │   │
│   │   ├── activation.rs         # Agent switching, window/pane focus logic
│   │   ├── monitor.rs            # Process detection, Herdr panel sync
│   │   ├── agent_detection.rs    # OS-level process matching utilities
│   │   ├── agent_preferences.rs  # Per-agent monitoring switches (agents.json)
│   │   │
│   │   ├── plugin.rs             # Plugin system (manifest loading, validation)
│   │   ├── agent_actions.rs      # ActionPlan, ActionCatalog, key binding model
│   │   ├── keymap.rs             # Resolves profile overrides → executable plans
│   │   ├── profile.rs            # Profile config (JSON persistence)
│   │   │
│   │   ├── hooks.rs              # Hook event processing (codex/claude/qwen)
│   │   ├── hook_integration.rs   # Hook install/uninstall into agent configs
│   │   │
│   │   ├── hardware_hid.rs       # macOS CGEventTap for ESP32 keyboard HID
│   │   ├── vendor_hid.rs         # Vendor HID (Report ID 6) deck sync
│   │   ├── macos_input.rs        # CGEvent key injection, accessibility
│   │   │
│   │   ├── herdr.rs              # Herdr terminal multiplexer integration
│   │   ├── tmux.rs               # Tmux session/pane management
│   │   ├── chatgpt_state.rs      # ChatGPT reasoning level sync
│   │   ├── dsh.rs                # DeepSeek Harness status polling
│   │   ├── dsh_plugin.rs         # Install dsh-deck-bridge into a DSH profile
│   │   │
│   │   ├── tray.rs               # System tray menu builder
│   │   ├── function_hint.rs      # Function key hint overlay window
│   │   └── vendor_hid.rs         # ESP32 deck view push (LEDs/labels)
│   │
│   └── plugins/                  # Plugin manifests (compiled into binary)
│       ├── builtin/
│       │   ├── chatgpt/plugin.json
│       │   ├── codex/plugin.json
│       │   ├── claude/plugin.json
│       │   ├── qwen/plugin.json
│       │   ├── herdr/plugin.json
│       │   └── dsh/plugin.json
│       └── opencode/plugin.json  # Bundled but non-built-in
│
└── plugins/                      # Same plugins at project root (symlinked/included)
```

---

## Core State Model (`state.rs`)

### `AppRuntime` — Mutable Application State

The central state container, wrapped in `Arc` and shared across threads:

```
AppRuntime
├── snapshot: RwLock<AppSnapshot>   # Read-heavy UI state
├── config: Mutex<AppConfig>        # Profile configuration
├── overrides: Mutex<HashMap<…>>    # Manual status overrides (mock)
├── hook_statuses: Mutex<HashMap<…>># Latest hook-reported status per agent
├── plugins: Arc<PluginRegistry>    # Loaded plugin manifests
├── deck_push / deck_rev            # Vendor HID deck sync channels
├── hint_generation: AtomicU64      # Function hint window lifecycle
├── focus_generation: AtomicU64     # Debounce concurrent focus requests
└── reasoning_sync: Mutex<()>       # Serialize ChatGPT encoder sync
```

### `AppSnapshot` — UI-Facing State (serialized to frontend)

```
AppSnapshot
├── agents: Vec<AgentState>         # All known agents + their status
├── agent_slots: Vec<AgentSlotState># A1–A5 → agent assignment
├── jobs: Vec<AgentJob>             # Recent hook jobs (max 50)
├── active_panels: Vec<ActivePanel> # Live session panels
├── sessions: Vec<AgentSession>     # Top 3 sessions for active agent
├── active_agent: Option<String>    # Currently selected agent
├── active_surface: Option<AgentSurface>  # desktop/terminal/herdr_pane/web
├── function_keys: Vec<FunctionKeyState>  # K1–K10 labels + actions
├── action_catalog: Vec<ActionCatalogItem># Configurable actions
├── device: DeviceState             # Hardware connection + encoder state
├── hint_preferences: HintPreferences
├── attention_count / attention_urgent  # Unread agent badges
└── herdr: HerdrSnapshot            # Herdr multiplexer state (macOS)
```

### Agent Status Lifecycle

```
Offline → Idle → Working → Done
                  ↓          ↑
              NeedsInput ────┘
                  ↓
                Error
```

Normalized statuses: `offline`, `idle`, `working`, `done`, `needs_input`, `error`, `unknown`

---

## Module Responsibilities

### Event Processing Pipeline

```
Hardware HID ──┐
Keyboard HID ──┤
HTTP API ──────┼──► process_event() ──► state mutation ──► emit_state()
Tauri cmd ─────┤                              │
Hooks ─────────┘                              ▼
                                        app.emit("state_changed")
                                        vendor_hid deck push
                                        tray icon update
```

**`lib.rs` — `process_event()`** is the single entry point for all hardware input:
- `key_down` / `agent_key` → Agent slot selection (A1–A5) or function key execution (K1–K10)
- `encoder` → Reasoning level adjustment (ChatGPT integration)

### Agent Detection & Monitoring

| Module | Role |
|--------|------|
| `monitor.rs` | Periodic process table refresh (60s), DSH status poll (5s), Herdr panel sync |
| `agent_detection.rs` | Process name/argv matching, terminal window focus via AppleScript |
| `activation.rs` | Agent switching: resolves surface, finds Herdr/tmux targets, focuses windows |
| `agent_preferences.rs` | `agents.json` monitoring switches, all agents on by default |

**Per-agent monitoring switch.** `AgentState.enabled` is the runtime truth; the
Settings → Agent monitoring list writes it through `set_agent_enabled`, which
persists `agents.json` and applies it immediately. A disabled agent keeps its
A1–A5 slot binding but is skipped by process detection and the DSH poller, has
its panels and jobs retired, ignores incoming hook events, and cannot be
activated from the deck. `state::prune_disabled_agents` runs on every
projection, so no writer can reintroduce a disabled agent's rows. `companion`
(the app itself) is not switchable.

**DSH diagnostics.** `dsh_diagnostics` is a one-shot, read-only probe for the
Settings page: DSH home, credentials, whether a `dsh` process is alive, the
`web` URL, sessions read from DSH storage, the Connection RPC answer, and the
`dsh-deck-bridge` tab report. A missing server or bridge is a reported field,
not an error.

**DSH push (event-driven).** `dsh-deck-bridge` subscribes to DSH's own
`api-session/*` events and POSTs `/api/dsh/push` with the hook token whenever the
session set changes. The push is a trigger, not state: `http_api::http_dsh_push`
calls the same `monitor::poll_dsh_status` sampler the poller uses, so DSH
normalisation stays in one place and a dropped push costs latency, never
correctness. While pushes are fresh (< 30s) the 5-second poll relaxes to 30s.

### Plugin System (`plugin.rs`)

Plugins are declarative JSON manifests — no shell commands or executable paths:

```
PluginManifest
├── id, display_name, built_in
├── detection: { process_names, argv_patterns }
├── surfaces: [desktop, terminal, herdr_pane, web]
├── keymap: { surface: { K1–K10: KeyBinding } }
└── hooks: { config_relative, events, handler_format }
```

**Plugin actions** (safe, allowlisted):
- `active_panel` — Focus a session panel
- `shortcut` — Keyboard shortcut (key + modifiers)
- `text_input` — Type text into terminal
- `herdr_prefix` — Herdr prefix key + shortcut
- `leader` — Leader sequence (e.g. tmux prefix)
- ChatGPT-specific: `fork`, `voice_hold`, `approve`, `reject`

**Plugin loading order:**
1. Built-in manifests (compiled via `include_str!`)
2. OpenCode (bundled but non-built-in)
3. User plugins from `~/.config/Agent Deck Companion/plugins/` and `~/.agent-deck-companion/plugins/`

### Key Resolution (`keymap.rs`)

Resolves physical K1–K10 keys into executable `ActionPlan`s:

```
Physical key (K3)
    → Profile override? → ConfiguredAction
    → Plugin manifest?  → PluginAction
    → Built-in default? → ResolvedAgentBinding
    → ActionPlan (shortcut / text_input / leader_sequence / …)
```

### Hook System (`hooks.rs`, `hook_integration.rs`)

Hooks are lifecycle event bridges injected into agent config files:

- **Supported agents:** codex, claude, qwen, qoder, opencode
- **Events:** SessionStart, UserPromptSubmit, PreToolUse, PermissionRequest, Stop, …
- **Transport:** HTTP POST to `127.0.0.1:34115/api/hooks/{agent_id}` with `x-agent-deck-token`
- **Install:** Writes hook config into agent's config file (e.g. `~/.codex/config.toml`)
- **Result:** Creates/updates `AgentJob` and `ActivePanel` entries in state

The Hooks page lists one extra entry with `kind: "plugin"`: `deepseek-harness`, which installs
`dsh-deck-bridge` into the running DSH profile (`dsh_plugin.rs`). DSH's plugin lives in the profile's
`package.json` + `dsh.profile.bundles`, so the companion runs DSH's own `npx @deepseek-ai/dsh plugin
--profile <name> add link:<bundled dir>` instead of editing those files. The bundled directory comes
from `bundle.resources` in `tauri.conf.json`; a development build falls back to the repository copy.
Only `hooks` entries get the mock-event developer controls.

---

## Communication Channels

### 1. Tauri Commands (UI → Rust)

| Command | Purpose |
|---------|---------|
| `get_state` | Poll current AppSnapshot |
| `send_hardware_event` | Simulate key/encoder input |
| `assign_agent_slot` | Map A-key to agent |
| `set_agent_enabled` | Turn an agent's monitoring on/off (persisted) |
| `dsh_diagnostics` | One-shot DSH integration probe (Settings) |
| `set_key_binding` | Customize K-key action |
| `reset_key_binding` | Restore K-key default |
| `herdr_agent_action` | Focus/continue/stop Herdr pane |
| `list_hook_integrations` | Show hook install status (plus the DSH plugin entry) |
| `install_agent_hooks` | Install hooks for agent |
| `install_dsh_plugin` / `uninstall_dsh_plugin` | Install `dsh-deck-bridge` into the running DSH profile |
| `set_hint_preferences` | Configure function hint overlay |

### 2. Tauri Events (Rust → UI)

| Event | Payload |
|-------|---------|
| `state_changed` | Full `AppSnapshot` |

### 3. HTTP API (`127.0.0.1:34115`)

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Health check |
| `/api/state` | GET | Full state snapshot |
| `/api/hooks/{agent_id}` | POST | Hook events (with token auth) |
| `/api/dsh/push` | POST | DSH plugin "look now" trigger (token auth, re-samples DSH) |
| `/api/hardware/key` | POST | Simulate key press |
| `/api/hardware/encoder` | POST | Simulate encoder rotation |
| `/api/mock/hooks/{agent_id}` | POST | Mock hook (no auth) |
| `/api/mock/status` | POST | Override agent status |
| `/api/mock/reset` | POST | Clear all overrides |
| `/api/hook-integrations` | GET | List hook integration states |

### 4. Hardware HID

| Path | Protocol | Direction |
|------|----------|-----------|
| Keyboard HID (F13–F17) | CGEventTap | ESP32 → Mac (A/K keys) |
| Vendor HID (Report ID 6) | hidapi | Bidirectional (events + deck view) |

---

## Frontend Architecture

Single-page SvelteKit app with two view modes:

1. **Main Companion Window** — Agent status, hardware simulator, hook management, key configuration
2. **Function Hint Overlay** — Floating K1–K10 reference card (separate Tauri window)

### Key Files

| File | Role |
|------|------|
| `+page.svelte` | All UI logic (~750 lines), LCD display, agent list, deck simulator |
| `api.ts` | Dual-mode API: Tauri `invoke()` when in webview, HTTP fetch as fallback |
| `types.ts` | TypeScript mirrors of Rust state types |

### State Flow

```
onMount() → getState() → snapshot
         → listen("state_changed") → live updates
         → setInterval(refresh, 1500) → polling fallback
```

---

## Profile Configuration (`profile.rs`)

Persisted to `~/.config/Agent Deck Companion/profile.json`:

```
AppConfig (version 3)
├── active_profile: String
└── profiles: { name: Profile }
    └── Profile
        ├── agent_slots: { A1–A5 → agent_id }
        ├── global_keys: { K1–K10 → ConfiguredAction }
        └── agent_keys: { agent_id → { surface → { key → ConfiguredAction } } }
```

`ConfiguredAction` variants:
- `agent_action { action_id }` — Plugin or built-in action
- `system_action { action_id }` — System-level (e.g. voice input)
- `keyboard_shortcut { key_code, modifiers }` — Direct key combo
- `disabled` — Key disabled for this agent/surface

---

## Platform-Specific Code

All macOS-gated code uses `#[cfg(target_os = "macos")]`:

| Module | macOS APIs Used |
|--------|----------------|
| `hardware_hid.rs` | CGEventTap (keyboard event interception) |
| `macos_input.rs` | CGEvent (key injection), Accessibility API |
| `herdr.rs` | Unix sockets (Herdr IPC) |
| `tmux.rs` | tmux command protocol |
| `agent_detection.rs` | AppleScript (window focus), process inspection |
| `activation.rs` | `open -b` (app launch), AppleScript (browser tab focus) |

---

## Built-in Agents

| Agent ID | Display Name | Surface | Detection |
|----------|-------------|---------|-----------|
| `qwen` | Qwen Code | Terminal | Process name matching |
| `herdr` | Herdr | HerdrPane | Herdr IPC socket |
| `codex` | Codex CLI | Terminal | Process name matching |
| `chatgpt` | ChatGPT | Desktop | macOS app bundle |
| `claude` | Claude Code | Terminal | Process name matching |
| `dsh` | DeepSeek Harness | Web/Terminal | Process detection (`dsh` / `@deepseek-ai/dsh`) + projection-cache polling |

Every built-in agent is monitored by default and can be switched off in
Settings; see *Per-agent monitoring switch* above. Non-built-in plugins
(`opencode`, `qoder`) are appended to the same list and are switchable too.

---

## Background Threads

| Thread | Interval | Purpose |
|--------|----------|---------|
| Process monitor | 60s | Refresh sysinfo process table, reconcile agent status |
| DSH poller | 5s (30s while pushes are fresh) | Re-sample DSH sessions; the deck plugin's push is the primary trigger |
| Herdr subscriber | Event-driven | Subscribe to Herdr state changes (with exponential backoff reconnect) |
| Herdr fallback | 30s | Poll Herdr state as backup |
| HTTP API | Persistent | axum server on `127.0.0.1:34115` |
| Hardware HID | Persistent | CGEventTap for keyboard HID (macOS) |
| Vendor HID | Persistent | HID device polling + deck view push |
| ChatGPT reasoning | On-demand | Sync encoder knob with ChatGPT reasoning level |

---

## Verification Commands

```bash
npm run check          # svelte-check (TypeScript)
npm run build          # Vite production build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
```
