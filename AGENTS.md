# AGENTS.md

## Repository purpose

This repository implements a local-first desktop companion for AI coding agents. Keep it a focused control surface; do not turn it into an IDE. See `ARCHITECTURE.md` for how the pieces fit together.

## POC architecture

- `src/`: SvelteKit + TypeScript presentation layer.
- `src-tauri/`: Tauri 2 shell and Rust core.
- Rust owns process/device state. The UI renders snapshots and sends semantic hardware events.
- Real serial input and HTTP mock input must pass through the same event handler.
- Agent-specific behavior belongs behind adapter-shaped boundaries, not in Svelte components.

## Development rules

- Keep the app local-first. Mock HTTP services must bind to loopback only.
- Never execute arbitrary shell commands received from imported profiles or hardware input.
- Preserve the normalized statuses defined by the code: offline, idle, working, done, needs_input, error, unknown.
- Treat top keys as Agent selectors and bottom keys as configurable actions.
- Avoid hard-coding a production USB protocol until VID/PID, framing, versioning, and checksums are agreed with firmware.
- Platform-specific window behavior must be isolated and must fail without crashing the app.
- Use Rust for OS/process/serial work and TypeScript for UI concerns.

## Verification

Before handing off a change, run the checks relevant to the touched area:

```bash
npm run check
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
```

For event-path changes, also exercise `/api/hardware/key` and `/api/mock/status` while the Tauri app is running.

## Scope discipline

The current milestone is a macOS-first POC covering ChatGPT Desktop / Codex, Claude Code, Qwen Code, Codex CLI, Herdr, DeepSeek Harness and OpenCode / Qoder: process monitoring, agent switching, session navigation, serial selection, and mock hardware input. Prefer small verifiable steps over building all modules from the full specification at once.

