#!/usr/bin/env bash
# Tests for the release version gate.
#
# `check-release-version.sh` takes the tag and both manifest paths as
# arguments rather than reading the real files and $GITHUB_REF, which is what
# makes it testable: a fixture pair stands in for the repository, so every
# branch below runs with no tag, no CI, and no edits to the real manifests.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIX="$HERE/fixtures"
CHECK="$HERE/../../check-release-version.sh"

pass=0
fail=0

# assert_exit <expected-code> <description> -- <command...>
assert_exit() {
  local expected="$1" desc="$2"; shift 3
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  if [ "$rc" -eq "$expected" ]; then
    pass=$((pass + 1)); echo "  ok   $desc"
  else
    fail=$((fail + 1))
    echo "  FAIL $desc (expected exit $expected, got $rc)"
    echo "$out" | sed 's/^/         /'
  fi
}

# assert_output <regex> <description> -- <command...>
assert_output() {
  local pattern="$1" desc="$2"; shift 3
  local out
  out="$("$@" 2>&1)"
  if echo "$out" | grep -qE "$pattern"; then
    pass=$((pass + 1)); echo "  ok   $desc"
  else
    fail=$((fail + 1))
    echo "  FAIL $desc (output did not match /$pattern/)"
    echo "$out" | sed 's/^/         /'
  fi
}

echo "check-release-version.sh"

assert_exit 0 "a tag matching both manifests passes" -- \
  "$CHECK" 0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

# The failure this whole script exists to prevent: a tag that does not match
# ships an installer whose in-app version disagrees with its Release page,
# and only a user ever notices.
assert_exit 1 "a tag ahead of the manifests fails" -- \
  "$CHECK" 0.2.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

assert_exit 1 "manifests disagreeing with each other fails" -- \
  "$CHECK" 0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.2.0.toml"

assert_output "0\.2\.0" "the message names the version that disagrees" -- \
  "$CHECK" 0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.2.0.toml"

# Clean semver is the convention -- it is what the manifests already carry, so
# there is no prefix to add or strip anywhere. Accepting `v0.1.0` as well would
# let two tag styles into the release history.
assert_exit 1 "a v-prefixed tag is rejected" -- \
  "$CHECK" v0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

assert_output "without" "the v-prefix message explains the convention" -- \
  "$CHECK" v0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

assert_exit 1 "a tag that is not a version at all is rejected" -- \
  "$CHECK" nightly "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

# The trigger fires on anything starting with a digit, so a date-shaped tag
# reaches this gate and must be refused rather than built as a release.
assert_exit 1 "a date-shaped tag is rejected" -- \
  "$CHECK" 2026-09-07 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0.toml"

# A prerelease tag has to either work or be refused clearly; silently
# building it as if it were the release is the one unacceptable outcome.
assert_exit 0 "a prerelease tag matching both manifests passes" -- \
  "$CHECK" 0.2.0-rc.1 "$FIX/tauri-0.2.0-rc.1.json" "$FIX/cargo-0.2.0-rc.1.toml"

# `version` appears inside tauri.conf.json's bundle and plugin blocks in real
# configs; a naive grep picks up the wrong one.
assert_exit 0 "the tauri version is read from the top level, not a nested block" -- \
  "$CHECK" 0.1.0 "$FIX/tauri-0.1.0-nested-decoys.json" "$FIX/cargo-0.1.0.toml"

# Cargo.toml has a [dependencies] section full of `version = "..."` lines.
assert_exit 0 "the cargo version is read from [package], not a dependency" -- \
  "$CHECK" 0.1.0 "$FIX/tauri-0.1.0.json" "$FIX/cargo-0.1.0-with-deps.toml"

assert_exit 1 "a missing manifest fails loudly rather than passing" -- \
  "$CHECK" 0.1.0 "$FIX/does-not-exist.json" "$FIX/cargo-0.1.0.toml"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
