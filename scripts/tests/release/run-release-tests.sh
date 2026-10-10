#!/usr/bin/env bash
# Tests for scripts/release/ (label checks, version bumps, release planning).
#
# Standard library only. `scripts/` goes on PYTHONPATH so the tests import
# `release` the same way `python3 scripts/release` does, and the tests'
# own directory so they can share helpers.py.
#
#   ./scripts/tests/release/run-release-tests.sh        # all
#   ./scripts/tests/release/run-release-tests.sh -v     # verbose
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

# No __pycache__ in the working tree.
export PYTHONDONTWRITEBYTECODE=1

# GitHub Actions sets these for every step. The commands under test append to
# them (a "Release plan" summary, target_sha=... outputs), so a suite that
# inherits them posts its fixture plans into the real job's summary: the
# `test-ubuntu` job showed "promoting uai-app-0.1.2" for a release that does not
# exist. test_cli.py::SuiteIsolationTests fails if the suite is run another way.
unset GITHUB_STEP_SUMMARY GITHUB_OUTPUT GITHUB_ENV GITHUB_PATH

PYTHONPATH="$ROOT/scripts:$HERE" exec python3 -m unittest discover -s "$HERE" -p 'test_*.py' "$@"
