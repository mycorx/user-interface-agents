#!/usr/bin/env bash
# Regenerates NOTICE.txt from the actual dependency graph.
#
# NOTICE.txt is the third-party attribution file: the PolyForm licenses in
# LICENSE.md cover UIA's own code only, and every bundled crate and npm
# package keeps its own license. Run this after any dependency change and
# commit the result.
#
#   ./scripts/gen-notice.sh            # merge this platform in (default)
#   ./scripts/gen-notice.sh --rewrite  # regenerate from scratch
#   ./scripts/gen-notice.sh --check    # fail if a dependency is unattributed (CI)
#
# Run it anywhere. That is the point of the default being a merge.
#
# The two halves do not behave the same way. `cargo metadata` runs without
# `--filter-platform`, so the crate half is platform-independent and is
# rewritten every time. The npm half comes from `pnpm licenses list`, which
# reads the *installed* store — so a Windows checkout simply does not have
# `@esbuild/linux-x64` and its siblings, and a Linux one does not have the
# win32 binaries. NOTICE.txt is meant to be the union of both.
#
# `--check` only fails on what is missing: entries it cannot find locally are
# reported as "expected — other platforms' binaries" and pass. That asymmetry
# used to be a trap, because the default was a full rewrite — a regeneration
# that dropped four packages still passed `--check` on the machine that did it.
# The default now merges the npm half into what NOTICE.txt already records, so
# regenerating is additive wherever it is run, and the script says out loud
# which entries it carried.
#
# `--rewrite` is the escape hatch for a package removed on purpose. It drops
# whatever is not installed here, so run it on Linux and read the diff.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'USAGE'
Usage: ./scripts/gen-notice.sh [--merge | --rewrite | --check]

  --merge     (default) regenerate NOTICE.txt, keeping the npm entries only
              another platform can see
  --rewrite   regenerate from scratch, dropping anything not installed here
  --check     fail if a dependency in the graph is unattributed (CI)
USAGE
}

# Argument parsing comes before cargo and pnpm run, so a mistyped flag costs
# nothing. It used to test only for `--check` and treat every other argument,
# including a typo, as "rewrite the file".
case "${1:-}" in
  "" | --merge) mode=merge ;;
  --rewrite)    mode=rewrite ;;
  --check)      mode=check ;;
  -h | --help)  usage; exit 0 ;;
  *)
    echo "gen-notice: unknown option '$1'" >&2
    usage >&2
    exit 2
    ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo metadata --format-version 1 --all-features >"$work/cargo.json"

# `pnpm licenses list` reads the installed store, so node_modules has to exist.
# Without it the npm half would silently come back empty — which in --check
# would pass by attributing nothing, the one failure mode worth refusing.
if ! pnpm licenses list --json >"$work/npm.json" 2>/dev/null; then
  echo "FAIL: 'pnpm licenses list' failed — run 'pnpm install' first" >&2
  exit 1
fi

# Rendering to the scratch directory and moving it into place keeps NOTICE.txt
# intact if the render fails — and `merge` reads the file it is about to
# replace, so it must still be there while python runs.
case "$mode" in
  check)
    python3 scripts/gen_notice.py check "$work/cargo.json" "$work/npm.json" NOTICE.txt
    ;;
  merge)
    python3 scripts/gen_notice.py merge "$work/cargo.json" "$work/npm.json" NOTICE.txt \
      >"$work/NOTICE.txt"
    mv "$work/NOTICE.txt" NOTICE.txt
    echo "wrote NOTICE.txt (merged)"
    ;;
  rewrite)
    python3 scripts/gen_notice.py render "$work/cargo.json" "$work/npm.json" >"$work/NOTICE.txt"
    mv "$work/NOTICE.txt" NOTICE.txt
    echo "wrote NOTICE.txt (rewritten — check the diff for removals)"
    ;;
esac
