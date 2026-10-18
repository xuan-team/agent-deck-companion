# Security Policy

## Reporting a vulnerability

Please **do not** report security issues through public GitHub issues or pull requests.

Instead, use GitHub's private vulnerability reporting:

1. Open the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Describe the issue, the impact, and steps to reproduce it.

If you cannot use GitHub, contact the maintainers privately through the organization profile at
<https://github.com/xuan-team>.

Please include:

- Affected version or commit.
- macOS version, and the agent / terminal involved.
- A minimal reproduction.
- Any logs, with tokens, personal paths and session content removed.

We will acknowledge reports as quickly as we can and keep you informed while we investigate.

## Scope notes

Agent Deck Companion is local-first. Things worth knowing when assessing an issue:

- The local HTTP API binds to `127.0.0.1:34115` only. Hooks authenticate with a local token stored in
  `~/.agent-deck-companion/hook-token`.
- The hook bridge records metadata only (session/task identifiers, working directory, event name,
  timestamps); it does not persist prompts, tool input, tool responses or transcripts.
- Hardware and profile input is treated as untrusted and must never be executed as an arbitrary shell
  command.

Do not forward the local debugging API to a public network, and do not paste tokens, private paths or
session content into a report.
