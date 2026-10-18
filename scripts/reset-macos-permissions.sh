#!/bin/bash
# Only reset this app's two TCC grants; never edit the TCC database.
set -euo pipefail
bundle_id=dev.agentdeck.companion
mode="${1:---dry-run}"
case "$mode" in
  --dry-run) ;;
  --reset) ;;
  *) echo "Usage: bash scripts/reset-macos-permissions.sh [--dry-run|--reset]" >&2; exit 2 ;;
esac
if [[ $# -gt 1 ]]; then echo "Unexpected arguments" >&2; exit 2; fi
if [[ "$mode" == --dry-run ]]; then
  echo "Quit Agent Deck Companion before resetting. Commands that --reset will run:"
  echo "/usr/bin/tccutil reset Accessibility $bundle_id"
  echo "/usr/bin/tccutil reset ListenEvent $bundle_id"
  echo "Then reopen the app at its stable install location and grant both permissions."
  exit 0
fi
if [[ "$(uname -s)" != Darwin ]]; then echo "macOS only" >&2; exit 1; fi
if /usr/bin/pgrep -x agent-deck-companion >/dev/null ||
   /usr/bin/pgrep -f '/Agent Deck Companion[.]app/Contents/MacOS/agent-deck-companion([[:space:]]|$)' >/dev/null; then
  echo "Quit Agent Deck Companion, then run --reset again." >&2
  exit 1
fi
/usr/bin/tccutil reset Accessibility "$bundle_id"
/usr/bin/tccutil reset ListenEvent "$bundle_id"
echo "Reset complete. Reopen Agent Deck Companion; use its macOS permissions panel to request access."
echo "macOS still requires you to enable each grant in Privacy settings."
