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
PYTHONPATH="$ROOT/scripts:$HERE" exec python3 -m unittest discover -s "$HERE" -p 'test_*.py' "$@"
