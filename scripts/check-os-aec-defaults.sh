#!/usr/bin/env bash
# Each desktop OS must get its own native echo cancellation by default, and
# Linux must get neither. The Windows and macOS halves are the whole point
# (S15 shipped a correct AEC wrapper that nothing ever called, because turning
# it on was a flag someone had to remember); the Linux half is PLAN.md's
# frozen gate that a plain `cargo build --workspace` never needs a toolchain
# the Linux sandbox lacks.
#
# `cargo tree --target` resolves target-cfg predicates without needing that
# OS, so this runs anywhere, including Ubuntu CI.
set -euo pipefail

# Captured, not piped into `grep -q`: a failing `cargo tree` must stop the
# check loudly, never read as "feature absent" and let `refuse` pass.
features_for() {
  local out
  if ! out="$(cargo tree -p uia-app --target "$1" -e features)"; then
    echo "FAIL: \`cargo tree -p uia-app --target $1 -e features\` failed;" >&2
    echo "      cannot tell which echo cancellation $1 gets" >&2
    exit 1
  fi
  printf '%s\n' "$out"
}

has_feature() {
  local target="$1" feature="$2" tree
  # Explicit: `set -e` does not apply inside an `if` condition, and the
  # `exit` in `features_for` only leaves its command-substitution subshell.
  tree="$(features_for "$target")" || exit 1
  grep -qF "uia-audio feature \"$feature\"" <<<"$tree"
}

expect() {
  local target="$1" feature="$2"
  if ! has_feature "$target" "$feature"; then
    echo "FAIL: $feature is not active for $target; echo cancellation would"
    echo "      silently not ship (see crates/uia-app/Cargo.toml's target stanzas)"
    exit 1
  fi
}

refuse() {
  local target="$1" feature="$2"
  if has_feature "$target" "$feature"; then
    echo "FAIL: $feature leaked into $target"
    exit 1
  fi
}

expect x86_64-pc-windows-msvc wasapi-aec
refuse x86_64-pc-windows-msvc coreaudio-aec
expect aarch64-apple-darwin coreaudio-aec
expect x86_64-apple-darwin coreaudio-aec
refuse aarch64-apple-darwin wasapi-aec
refuse x86_64-unknown-linux-gnu wasapi-aec
refuse x86_64-unknown-linux-gnu coreaudio-aec

echo "OK: wasapi-aec is Windows-only, coreaudio-aec is macOS-only, Linux has neither"
