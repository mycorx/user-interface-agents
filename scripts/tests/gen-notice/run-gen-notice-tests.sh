#!/usr/bin/env bash
# Tests for the NOTICE.txt merge mode.
#
# `gen_notice.py` takes both dependency dumps as file arguments rather than
# shelling out to cargo and pnpm itself, which is what makes it testable: a
# fixture pair stands in for a machine, so every case below runs with no
# toolchain, no node_modules and no edits to the real NOTICE.txt.
#
# The fixtures are two views of one project:
#
#   *-union.json   what a Windows machine last saw, and what NOTICE-union.txt
#                  was rendered from -- it carries `@esbuild/win32-x64` and
#                  `lightningcss-win32-x64-msvc`.
#   *-linux.json   what this machine sees now: the win32 binaries are simply
#                  not installed, `vite` is new, `typescript` moved 5.6 -> 5.7.
#                  The crate half dropped `windows-sys` and gained `chrono-tz`.
#
# The whole point of merge mode is that regenerating from the linux view must
# not write the win32 packages out of the file.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIX="$HERE/fixtures"
GEN="$HERE/../../gen_notice.py"
SH="$HERE/../../gen-notice.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
fail=0

# Both matchers require the command to have SUCCEEDED. Without that, every
# assert_lacks passes the moment the command dies -- an absent mode would look
# like a suite full of green rather than a suite that never ran.
#
# `grep -- "$pattern"` because a pattern like `--rewrite` is otherwise read as
# a grep option.

# assert_contains <regex> <description> -- <command...>
assert_contains() {
  local pattern="$1" desc="$2"; shift 3
  local out rc
  out="$("$@" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ]; then
    fail=$((fail + 1)); echo "  FAIL $desc (command exited $rc)"
    "$@" 2>&1 >/dev/null | sed 's/^/         /'
  elif echo "$out" | grep -qE -- "$pattern"; then
    pass=$((pass + 1)); echo "  ok   $desc"
  else
    fail=$((fail + 1))
    echo "  FAIL $desc (stdout did not match /$pattern/)"
    echo "$out" | sed 's/^/         /'
  fi
}

# assert_lacks <regex> <description> -- <command...>
assert_lacks() {
  local pattern="$1" desc="$2"; shift 3
  local out rc
  out="$("$@" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ]; then
    fail=$((fail + 1)); echo "  FAIL $desc (command exited $rc)"
    "$@" 2>&1 >/dev/null | sed 's/^/         /'
  elif echo "$out" | grep -qE -- "$pattern"; then
    fail=$((fail + 1))
    echo "  FAIL $desc (stdout matched /$pattern/ but should not have)"
    echo "$out" | sed 's/^/         /'
  else
    pass=$((pass + 1)); echo "  ok   $desc"
  fi
}

# assert_stderr <regex> <description> -- <command...>
assert_stderr() {
  local pattern="$1" desc="$2"; shift 3
  local out
  out="$("$@" 2>&1 >/dev/null)"
  if echo "$out" | grep -qE -- "$pattern"; then
    pass=$((pass + 1)); echo "  ok   $desc"
  else
    fail=$((fail + 1))
    echo "  FAIL $desc (stderr did not match /$pattern/)"
    echo "$out" | sed 's/^/         /'
  fi
}

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

merge_linux() {
  python3 "$GEN" merge "$FIX/cargo-linux.json" "$FIX/npm-linux.json" "$FIX/NOTICE-union.txt"
}

echo "gen_notice.py merge"

# The failure this whole mode exists to prevent. Regenerating on Linux used to
# write the four win32 packages out of NOTICE.txt, and --check called them
# "expected -- other platforms' binaries" on the way past.
assert_contains "@esbuild/win32-x64  +0\.25\.0" \
  "an npm package not installed here is carried over" -- \
  merge_linux

# A carried package keeps the license it was attributed under, even when no
# package on this platform declares that license -- the heading has to come
# back from the existing file, not from the fresh dump.
assert_contains "MPL-2\.0" \
  "a license heading no local package uses is recreated for a carried entry" -- \
  merge_linux

assert_contains "lightningcss-win32-x64-msvc  +1\.30\.0" \
  "the carried entry sits under its own license heading" -- \
  merge_linux

assert_contains "vite  +6\.0\.0" \
  "a package new on this platform is added" -- \
  merge_linux

# Carrying by (name, version) pair would accumulate every version a package has
# ever had. Carrying by name means a bump replaces, and only names absent from
# the fresh dump entirely are carried.
assert_contains "typescript  +5\.7\.0" \
  "a bumped npm package takes its fresh version" -- \
  merge_linux

assert_lacks "typescript  +5\.6\.0" \
  "the superseded version of a bumped npm package does not linger" -- \
  merge_linux

# `cargo metadata` runs without --filter-platform, so the crate half is already
# the union on every machine. Merging it would only let deleted crates rot in
# the file forever, so merge mode rewrites that half.
assert_lacks "windows-sys" \
  "a crate dropped from the graph is not carried, since the crate half is platform-independent" -- \
  merge_linux

assert_contains "chrono-tz  +0\.12\.1" \
  "a newly added crate is written" -- \
  merge_linux

assert_lacks "serde  +1\.0\.190" \
  "the crate half takes fresh versions, not the file's" -- \
  merge_linux

# A genuine removal is indistinguishable from another platform's binary, so the
# one thing merge mode must not do is stay silent about what it kept.
assert_stderr "@esbuild/win32-x64" \
  "the packages it carried are named, so a real removal is still visible" -- \
  merge_linux

# The npm heading counts the union it just wrote, not the local dump.
assert_contains "^5 packages, grouped" \
  "the npm count line counts the merged union" -- \
  merge_linux

# Bootstrap: a repository with no NOTICE.txt yet must still produce one.
assert_contains "chrono-tz" \
  "merging with no existing NOTICE.txt renders the fresh graph alone" -- \
  python3 "$GEN" merge "$FIX/cargo-linux.json" "$FIX/npm-linux.json" "$work/absent.txt"

echo
echo "merge output is well-formed"

merge_linux >"$work/merged.txt" 2>/dev/null

# Merging is what CI checks, so the two must agree: everything installed here
# has to be attributed in what merge just wrote.
assert_exit 0 "the merged file passes --check against this platform" -- \
  python3 "$GEN" check "$FIX/cargo-linux.json" "$FIX/npm-linux.json" "$work/merged.txt"

# Without this, every regeneration produces a diff and nobody can tell a real
# dependency change from reordering noise.
python3 "$GEN" merge "$FIX/cargo-linux.json" "$FIX/npm-linux.json" "$work/merged.txt" \
  >"$work/merged-twice.txt" 2>/dev/null
assert_exit 0 "the merge actually produced a file to compare" -- \
  test -s "$work/merged.txt"
assert_exit 0 "merging an already-merged file changes nothing" -- \
  diff -q "$work/merged.txt" "$work/merged-twice.txt"

echo
echo "render is still the escape hatch"

# Merge cannot be the only mode: a package removed on purpose has to be
# removable, and that is what `render` is for.
assert_lacks "@esbuild/win32-x64" \
  "render still drops what is not installed here" -- \
  python3 "$GEN" render "$FIX/cargo-linux.json" "$FIX/npm-linux.json"

echo
echo "gen-notice.sh argument handling"

# Argument parsing happens before cargo and pnpm run, so these cases need no
# toolchain. A mistyped flag silently rewriting NOTICE.txt is the failure here:
# the old script tested only for --check and treated everything else as "write".
assert_exit 2 "an unknown flag is refused rather than treated as a rewrite" -- \
  "$SH" --rewrit

assert_contains "--rewrite" "the usage message names the real flags" -- \
  "$SH" --help

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
