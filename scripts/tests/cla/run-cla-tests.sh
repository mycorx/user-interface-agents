#!/usr/bin/env bash
# Tests for the CLA acceptance gate and the contributor ledger updater.
#
# Both scripts under test take their JSON as file arguments rather than reading
# $GITHUB_EVENT_PATH directly, which is the whole reason they are testable: a
# fixture file stands in for a webhook payload, so every branch below runs with
# no network, no GitHub, and no repository state.
#
# The fixtures mirror the real shapes: the `pull_request` event payload, and
# the array returned by GET /repos/:o/:r/pulls/:n/commits.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIX="$HERE/fixtures"
CHECK="$HERE/../../check_cla_acceptance.py"
UPDATE="$HERE/../../update_contributor_ledger.py"

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

# assert_no_output <regex> <description> -- <command...>
assert_no_output() {
  local pattern="$1" desc="$2"; shift 3
  local out
  out="$("$@" 2>&1)"
  if echo "$out" | grep -qE "$pattern"; then
    fail=$((fail + 1))
    echo "  FAIL $desc (output unexpectedly matched /$pattern/)"
    echo "$out" | sed 's/^/         /'
  else
    pass=$((pass + 1)); echo "  ok   $desc"
  fi
}

echo "check_cla_acceptance.py"

assert_exit 0 "author already in the ledger passes without a trailer" -- \
  "$CHECK" "$FIX/event-ledgered.json" "$FIX/commits-no-trailer.json" "$FIX/ledger.md"

assert_exit 0 "a bot author is exempt" -- \
  "$CHECK" "$FIX/event-bot.json" "$FIX/commits-no-trailer.json" "$FIX/ledger.md"

assert_exit 0 "a newcomer with the trailer passes" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-with-trailer.json" "$FIX/ledger.md"

assert_exit 1 "a newcomer without the trailer fails" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-no-trailer.json" "$FIX/ledger.md"

assert_output "MycorX-CLA-Accepted" "the failure message tells them what to add" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-no-trailer.json" "$FIX/ledger.md"

assert_exit 1 "a trailer naming a different CLA version does not count" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-trailer-wrong-version.json" "$FIX/ledger.md"

assert_exit 1 "a malformed payload with no author fails closed" -- \
  "$CHECK" "$FIX/event-no-author.json" "$FIX/commits-no-trailer.json" "$FIX/ledger.md"

# The multi-author warning is advisory: commit authorship is unauthenticated,
# so it informs the maintainer rather than blocking the contributor.
assert_exit 0 "multiple commit authors still passes when the PR author accepted" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-multi-author.json" "$FIX/ledger.md"

assert_output "WARNING.*more than one" "multiple commit authors raises a warning" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-multi-author.json" "$FIX/ledger.md"

assert_output "hubot" "the warning names the unledgered co-author" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-multi-author.json" "$FIX/ledger.md"

assert_no_output "WARNING" "a single-author PR raises no warning" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-with-trailer.json" "$FIX/ledger.md"

# A commit whose email matches no GitHub account resolves `author` to null.
# It must not crash, and must not be silently credited to anyone.
assert_exit 0 "a commit with an unresolvable author does not crash the check" -- \
  "$CHECK" "$FIX/event-newcomer.json" "$FIX/commits-unresolved-author.json" "$FIX/ledger.md"

echo
echo "update_contributor_ledger.py"

# Each ledger test works on a scratch copy: the updater rewrites the file.
scratch_ledger() {
  local dest; dest="$(mktemp)"
  cp "$FIX/ledger.md" "$dest"; echo "$dest"
}

L1="$(scratch_ledger)"
assert_exit 0 "a merged newcomer is appended" -- \
  "$UPDATE" "$FIX/event-merged-newcomer.json" "$FIX/commits-with-trailer.json" "$L1"
assert_output "@octocat" "the appended row names the contributor" -- cat "$L1"
assert_output "v1\.0" "the appended row records the CLA version" -- cat "$L1"
assert_output "13" "the appended row records the pull request number" -- cat "$L1"

L2="$(scratch_ledger)"
assert_exit 0 "an unmerged close changes nothing" -- \
  "$UPDATE" "$FIX/event-closed-unmerged.json" "$FIX/commits-with-trailer.json" "$L2"
assert_exit 1 "...and really wrote nothing" -- grep -q "octocat" "$L2"

L3="$(scratch_ledger)"
assert_exit 0 "an already-ledgered author is not duplicated" -- \
  "$UPDATE" "$FIX/event-merged-ledgered.json" "$FIX/commits-with-trailer.json" "$L3"
assert_output "1" "exp4bra1n still appears exactly once" -- \
  bash -c "grep -c '@exp4bra1n' '$L3'"

# Running the same merge twice must not append twice: the workflow can be
# replayed, and a duplicated row would corrupt the record.
L4="$(scratch_ledger)"
"$UPDATE" "$FIX/event-merged-newcomer.json" "$FIX/commits-with-trailer.json" "$L4" >/dev/null 2>&1
assert_exit 0 "re-running the same merge is idempotent" -- \
  "$UPDATE" "$FIX/event-merged-newcomer.json" "$FIX/commits-with-trailer.json" "$L4"
assert_output "^1$" "octocat appears exactly once after two runs" -- \
  bash -c "grep -c '@octocat' '$L4'"

rm -f "$L1" "$L2" "$L3" "$L4"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
