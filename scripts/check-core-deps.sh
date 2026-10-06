#!/usr/bin/env bash
# uia-core must stay free of I/O and platform crates so it remains
# headlessly testable. See the spec's architectural rule.
set -euo pipefail
FORBIDDEN='cpal|^tauri|tokio-tungstenite|aws-sdk-|^webrtc|webrtc-audio-processing|^keyring'
MANIFEST="crates/uia-core/Cargo.toml"

# A missing manifest must fail loudly: `grep` exits 2 there, and inside an `if`
# that is indistinguishable from "no forbidden dep found" — which would turn a
# renamed or deleted crate into a silent pass.
if [[ ! -f "$MANIFEST" ]]; then
  echo "FAIL: $MANIFEST not found"
  exit 1
fi

if grep -nE "^\s*(${FORBIDDEN})" "$MANIFEST"; then
  echo "FAIL: uia-core depends on a forbidden crate (see spec architectural rule)"
  exit 1
fi
echo "OK: uia-core dependency rule satisfied"
