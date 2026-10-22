# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- GitHub Actions workflow that builds a universal macOS DMG on `v*` tags, publishes it as a GitHub
  Release, and includes install notes for the `xattr -dr com.apple.quarantine` step required by the
  ad-hoc signed build.

## [0.1.0]

First public release. Agent Deck Companion is a local-first, macOS-first desktop companion for AI
coding agents. It runs standalone — no AgentDeck hardware is required.

### Added

- Resident session panel listing sessions across monitored agents, with project grouping and three
  reorderable views (Needs action, Codex, Other agents).
- Click-to-open session navigation that returns to the exact terminal, pane, or ChatGPT/Codex thread
  via `codex://threads/<id>` deep links.
- Attention queue for sessions that need input, errored, or produced a new result, with read/unread
  state.
- In-panel alert cards and optional macOS desktop notifications that do not steal focus.
- Per-agent monitoring toggle (Settings → Agent Monitoring) with preserved A1–A5 slot bindings.
- Agent adapters for ChatGPT Desktop / Codex, Claude Code, Qwen Code, Codex CLI, Herdr,
  DeepSeek Harness (via the bundled `dsh-deck-bridge` plugin), and OpenCode / Qoder.
- Local hooks bridge that records status metadata only (no prompts, tool input, tool responses or
  transcripts), with explicit install/uninstall and `*.agent-deck-backup` backups.
- Optional ESP32 AgentDeck hardware support: A1–A5 agent selectors, K1–K10 actions, session
  navigation, an encoder knob, keyboard notification audio, and a hardware simulator for development.
- Local HTTP API bound to `127.0.0.1:34115` with local-token authentication for hooks.

[Unreleased]: https://github.com/xuan-team/agent-deck-companion/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/xuan-team/agent-deck-companion/releases/tag/v0.1.0
