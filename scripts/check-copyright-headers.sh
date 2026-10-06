#!/usr/bin/env bash
# Every source file carries a copyright header, so a file that leaves this
# repository still says who owns it and under what terms.
#
#   ./scripts/check-copyright-headers.sh        # report files missing it
#   ./scripts/check-copyright-headers.sh --fix  # prepend it where missing
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

LINE1='// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.'
LINE2='// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.'
MARKER='Copyright (c) 2026 MycorX'

fix=false
[[ "${1:-}" == "--fix" ]] && fix=true

# Generated bindings and vendored spikes are excluded: spikes/ are throwaway
# probes outside the workspace, and *.d.ts shims are not authored code.
mapfile -t files < <(
  find crates src -type f \( -name '*.rs' -o -name '*.ts' \) \
    -not -path '*/target/*' \
    -not -path '*/node_modules/*' \
    -not -name '*.d.ts' \
    | sort
)

missing=()
for f in "${files[@]}"; do
  if ! grep -qF "$MARKER" "$f"; then
    missing+=("$f")
  fi
done

if [[ ${#missing[@]} -eq 0 ]]; then
  echo "OK: all ${#files[@]} source files carry a copyright header"
  exit 0
fi

if [[ "$fix" == true ]]; then
  for f in "${missing[@]}"; do
    tmp="$(mktemp)"
    printf '%s\n%s\n\n' "$LINE1" "$LINE2" >"$tmp"
    cat "$f" >>"$tmp"
    mv "$tmp" "$f"
    echo "added header: $f"
  done
  echo "added copyright header to ${#missing[@]} file(s)"
  exit 0
fi

printf 'FAIL: %d file(s) missing a copyright header:\n' "${#missing[@]}"
printf '  %s\n' "${missing[@]}"
echo "Run ./scripts/check-copyright-headers.sh --fix"
exit 1
