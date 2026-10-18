# Contributing to Agent Deck Companion

Thanks for your interest in improving Agent Deck Companion. This document explains how to set up the
project, what we look for in changes, and how to get a pull request merged.

## Ways to contribute

- Report bugs and propose features through [GitHub Issues](https://github.com/xuan-team/agent-deck-companion/issues).
- Improve documentation (typos, unclear steps, missing agent notes).
- Add or fix an agent adapter, hook integration or plugin manifest.
- Improve the UI, accessibility or localization.

For security issues, do **not** open a public issue — follow [`SECURITY.md`](SECURITY.md).

## Development setup

Prerequisites:

- macOS (Apple Silicon validated)
- Node.js 20+ and npm
- Rust stable (with `cargo` and `rustfmt`)
- [Tauri 2 system dependencies](https://tauri.app/start/prerequisites/)

```bash
npm install
npm run tauri:dev
```

The Rust core owns process/device state; the SvelteKit frontend renders snapshots and sends semantic
hardware events. See [`ARCHITECTURE.md`](ARCHITECTURE.md) and [`AGENTS.md`](AGENTS.md) before making
structural changes.

## Before you open a pull request

Run the checks for the area you touched:

```bash
npm run check
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --manifest-path src-tauri/Cargo.toml
```

For event-path changes, also exercise `/api/hardware/key` and `/api/mock/status` while the Tauri app
is running. Running `npm run dev` alone still requires an existing Tauri backend for the local API.

Add or update tests where the behavior is testable (`cargo test --manifest-path src-tauri/Cargo.toml`,
plus the Node tests under `integrations/opencode-v2` and `dsh-deck-bridge`).

## Guidelines

- Keep the app a focused control surface, not an IDE.
- Real serial input and HTTP mock input must pass through the same event handler.
- Agent-specific behavior belongs behind adapter-shaped boundaries, not in Svelte components.
- Never execute arbitrary shell commands received from imported profiles or hardware input.
- Keep the app local-first. Mock HTTP services bind to loopback only.
- Preserve the normalized statuses: `offline`, `idle`, `working`, `done`, `needs_input`, `error`,
  `unknown`.
- Treat top keys as agent selectors and bottom keys as configurable actions.
- Do not hard-code a production USB protocol until VID/PID, framing, versioning and checksums are
  agreed with the firmware.

## Commit and pull request style

- Prefer small, focused commits with an imperative subject line (for example
  `fix: keep the window reopenable`).
- Describe **what** changed and **why**; include reproduction steps for fixes.
- Reference related issues.
- Keep pull requests scoped to one concern.

## Licensing

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
