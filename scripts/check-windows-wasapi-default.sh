#!/usr/bin/env bash
# Windows must get WASAPI-native echo cancellation by default, and no other
# platform may pay for it. Both halves matter: the Windows half is the whole
# point (see .plan/LEDGER.md — S15 shipped a correct AEC wrapper that nothing
# ever called, because turning it on was a flag someone had to remember), and
# the Linux half is PLAN.md's frozen sandbox gate, which says a plain
# `cargo build --workspace` must never need a toolchain the WSL2 sandbox lacks.
#
# `cargo tree --target` resolves target-cfg predicates without needing that OS,
# so this runs anywhere — including the Linux sandbox, before a Windows machine
# has ever seen the change.
set -euo pipefail

FEATURE='uia-audio feature "wasapi-aec"'

if ! cargo tree -p uia-app --target x86_64-pc-windows-msvc -e features \
  | grep -qF "$FEATURE"; then
  echo "FAIL: wasapi-aec is not active for the Windows target; echo cancellation"
  echo "      would silently not ship (see crates/uia-app/Cargo.toml's"
  echo "      [target.'cfg(windows)'.dependencies] stanza)"
  exit 1
fi

if cargo tree -p uia-app --target x86_64-unknown-linux-gnu -e features \
  | grep -qF "$FEATURE"; then
  echo "FAIL: wasapi-aec leaked into the Linux target; it pulls in the"
  echo "      windows crate and breaks the no-toolchain workspace gate"
  exit 1
fi

echo "OK: wasapi-aec is the Windows-only default"
