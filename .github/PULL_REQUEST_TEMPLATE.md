<!-- Thanks for contributing! -->

## What changed

<!-- A short description of the change. -->

## Why

<!-- The problem this solves or the motivation behind it. Link related issues. -->

Fixes #

## How it was tested

<!-- Commands run, agents/terminals exercised, screenshots or logs (with secrets removed). -->

- [ ] `npm run check`
- [ ] `npm run build`
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
- [ ] `cargo check --manifest-path src-tauri/Cargo.toml`

## Checklist

- [ ] The change keeps Companion a focused control surface, not an IDE.
- [ ] Agent-specific behavior stays behind adapter boundaries, not in Svelte components.
- [ ] No arbitrary shell commands can be triggered from imported profiles or hardware input.
- [ ] Local HTTP services still bind to loopback only.
- [ ] Normalized statuses are preserved (`offline`, `idle`, `working`, `done`, `needs_input`,
      `error`, `unknown`).
