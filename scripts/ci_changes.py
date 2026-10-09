#!/usr/bin/env python3
"""Decides whether a pull request changes anything the CI jobs check.

Run by the `changes` job in .github/workflows/ci.yaml, which skips the other
jobs when this prints `code=false`.

    ci_changes.py --event <github.event_name>

On a pull_request it diffs the merge commit against its first parent (the base
branch tip) and matches the files against every component's `code_paths` in
.github/release-components.json plus EXTRA_PATHS. Any other event (manual run,
release.yaml calling CI at the commit being shipped) always means `code=true`.

The result goes to $GITHUB_OUTPUT as `code=true|false` and to stdout.
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from release import paths, registry  # noqa: E402

# What CI runs or checks beyond the app's own code_paths. The registry is on the
# list so a PR cannot narrow the filter and skip the jobs in the same change.
EXTRA_PATHS = (
    ".github/workflows/**",
    registry.REGISTRY_PATH,
    "scripts/**",
    "NOTICE.txt",
    "LICENSES/**",
    "uia.example.toml",
)


def first_code_file(files, components):
    """The first changed file CI has to check, or None."""
    patterns = list(EXTRA_PATHS)
    for component in components.values():
        patterns.extend(component.code_paths)
    return next((f for f in files if any(paths.matches(p, f) for p in patterns)), None)


def changed_files(base="HEAD^1", head="HEAD"):
    out = subprocess.run(
        ["git", "diff", "--name-only", base, head],
        check=True, capture_output=True, text=True,
    ).stdout
    return out.splitlines()


def emit(code: bool, why: str) -> None:
    print(f"code={str(code).lower()}: {why}")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a") as f:
            f.write(f"code={str(code).lower()}\n")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="ci_changes")
    parser.add_argument("--event", required=True, help="the workflow's github.event_name")
    args = parser.parse_args(argv)

    if args.event != "pull_request":
        emit(True, f"{args.event} always runs the full suite")
        return 0

    components = registry.load_registry(registry.REGISTRY_PATH)
    files = changed_files()
    print("changed files:", *files, sep="\n  ")
    hit = first_code_file(files, components)
    if hit:
        emit(True, f"{hit} is a code path")
    else:
        emit(False, "no code, script or workflow changes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
