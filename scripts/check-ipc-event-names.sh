#!/usr/bin/env bash
# The Rust side emits IPC events by named const; the Svelte side listens for
# them as bare string literals. Nothing reconciles the two: rename one half and
# the workspace still builds, svelte-check still passes, and the HUD simply
# stops updating at runtime. That failure is silent and was the main risk in
# the rename to uia, so it gets a check of its own.
#
# Earlier versions of this check were themselves wrong, which is why it matches
# the way it does now:
#
#   - Scanning for 'uia://[a-z-]*' meant a listener renamed to 'uia://stateX'
#     fell out of the set instead of being reported. The check passed on the
#     exact drift it exists to catch. Hence: match any scheme, any suffix.
#   - Matching on listen(...) call syntax missed the multi-line
#     `listen<{...}>(\n  'uia://text-turn-support',` in HudCard.svelte, because
#     grep is line-based. Hence: match the literals, not the call.
#
# Event literals never contain a space; the console.error strings that mention
# them always do ("uia://state listen() failed:"), which is what separates the
# two without needing to parse the surrounding code. Ordinary web URLs are
# excluded by scheme -- they are content, not IPC.
set -euo pipefail

emitted=$(grep -rho '"uia://[^" ]*"' crates/ --include='*.rs' | tr -d '"' | sort -u)
listened=$(grep -rhoE "'[a-z][a-z0-9+.-]*://[^' ]*'" src/ \
  | tr -d "'" \
  | grep -vE '^(https?|file|data|blob|wss?)://' \
  | sort -u)

if [ -z "$emitted" ]; then
  echo "FAIL: no uia:// event constants found in crates/ -- has the scheme been renamed?"
  exit 1
fi
if [ -z "$listened" ]; then
  echo "FAIL: no event literals found in src/ -- has the listen() shape changed?"
  exit 1
fi

# Every event the frontend listens for must be one the backend actually emits.
# The reverse is deliberately allowed: Rust may emit an event no card consumes
# yet, which is how a new event lands before its UI does.
orphans=$(comm -23 <(echo "$listened") <(echo "$emitted") || true)
if [ -n "$orphans" ]; then
  echo "FAIL: the frontend references events no Rust code emits:"
  echo "$orphans" | sed 's/^/       /'
  echo "       (a listen() literal and its hud.rs const have drifted apart)"
  exit 1
fi

echo "OK: $(echo "$listened" | wc -l | tr -d ' ') frontend IPC listeners all match an emitted event"
