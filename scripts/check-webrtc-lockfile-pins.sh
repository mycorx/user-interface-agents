#!/usr/bin/env bash
# `webrtc` and `rtc` are pinned `=0.20.2` in crates/uia-openai/Cargo.toml
# because S9 shipped a live-only ICE-gathering regression in `webrtc` 0.20.3
# that no offline test caught. `rtc-media` belongs to the same version-locked
# family but is a transitive dependency Cargo gives us no way to pin directly;
# it is held at 0.20.3 by the committed lockfile alone. This script fails
# loudly if any member of the family drifts from the versions verified live,
# so a stray `cargo update` cannot silently reintroduce S9's regression.
set -euo pipefail

LOCKFILE="Cargo.lock"

# A missing lockfile must fail loudly rather than read as "nothing drifted" —
# same reasoning as check-core-deps.sh's missing-manifest guard.
if [[ ! -f "$LOCKFILE" ]]; then
  echo "FAIL: $LOCKFILE not found"
  exit 1
fi

# `tr -d '\r'` because this repo is developed on Windows with
# core.autocrlf=true, which leaves Cargo.lock CRLF in the working tree. Without
# it the `$` anchor below never matches locally and the script passes
# vacuously — the exact failure mode this guard exists to prevent.
#
# Fed via process substitution, not a `tr | awk` pipe: `awk`'s `exit` fires as
# soon as it finds the version line, and under `pipefail` a `tr` that still has
# unconsumed input when the pipe closes gets SIGPIPE, which turns into a
# spurious script-level failure ("Broken pipe") with no FAIL: diagnostic —
# timing-dependent on how much of the lockfile is left unread, so it can pass
# locally and fail in CI. Process substitution isn't a `|` pipeline, so
# `pipefail` never sees it.
locked_version() {
  awk -v crate="$1" '
    $0 == "name = \"" crate "\"" { found = 1; next }
    found && /^version = / { gsub(/^version = "|"$/, ""); print; exit }
  ' < <(tr -d '\r' <"$LOCKFILE")
}

expect_version() {
  local crate="$1" expected="$2" actual
  actual=$(locked_version "$crate")
  if [[ -z "$actual" ]]; then
    echo "FAIL: crate '${crate}' not found in $LOCKFILE"
    exit 1
  fi
  if [[ "$actual" != "$expected" ]]; then
    echo "FAIL: ${crate} locked at ${actual}, expected ${expected} (webrtc/rtc/rtc-media are version-locked together — see crates/uia-openai/Cargo.toml)"
    exit 1
  fi
}

expect_version "webrtc" "0.20.2"
expect_version "rtc" "0.20.2"
expect_version "rtc-media" "0.20.3"
echo "OK: webrtc/rtc/rtc-media lockfile pins unchanged"
