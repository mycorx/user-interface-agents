#!/usr/bin/env bash
# The release tag, tauri.conf.json and crates/uia-app/Cargo.toml must all name
# the same version.
#
# Without this, a mistyped tag produces installers whose in-app version
# disagrees with the Release page they hang off — and nothing in the build
# notices, because `tauri build` is perfectly happy to bundle 0.1.0 under a
# 0.2.0 tag. The person who finds out is a user reading an About box.
#
# Takes every path as an argument so it is testable against fixtures rather
# than only against the real repository (see scripts/tests/release-version/).
#
# Usage: check-release-version.sh <tag> [tauri.conf.json] [Cargo.toml]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TAG="${1:-}"
TAURI_CONF="${2:-$HERE/../crates/uia-app/tauri.conf.json}"
CARGO_TOML="${3:-$HERE/../crates/uia-app/Cargo.toml}"

if [ -z "$TAG" ]; then
  echo "FAIL: no tag given"
  echo "      usage: check-release-version.sh <tag> [tauri.conf.json] [Cargo.toml]"
  exit 1
fi

# Clean semver, no `v`. It is what both manifests already carry, so there is
# no prefix to add or strip at any point -- the tag IS the version. A
# `v`-prefixed tag is caught here rather than ignored, because the workflow
# deliberately triggers on it too: an old-habit `v0.2.0` should fail loudly
# instead of matching nothing and silently producing no release at all.
if [[ "$TAG" =~ ^v[0-9] ]]; then
  echo "FAIL: '$TAG' carries a 'v' prefix; tags are clean semver without one"
  echo "      use '${TAG#v}' instead"
  exit 1
fi

if [[ ! "$TAG" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "FAIL: '$TAG' is not a release tag"
  echo "      expected MAJOR.MINOR.PATCH, optionally with a -prerelease suffix"
  echo "      (e.g. 0.1.0 or 0.2.0-rc.1)"
  exit 1
fi
TAG_VERSION="$TAG"

for path in "$TAURI_CONF" "$CARGO_TOML"; do
  if [ ! -f "$path" ]; then
    echo "FAIL: $path does not exist"
    exit 1
  fi
done

# Parsed properly rather than grepped: `version` appears inside
# tauri.conf.json's bundle/plugin blocks and on every line of Cargo.toml's
# [dependencies], so a line-oriented match reads the wrong one about as often
# as the right one.
TAURI_VERSION="$(python3 -c '
import json, sys
with open(sys.argv[1]) as f:
    print(json.load(f).get("version", ""))
' "$TAURI_CONF")"

CARGO_VERSION="$(python3 -c '
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    print(tomllib.load(f).get("package", {}).get("version", ""))
' "$CARGO_TOML")"

status=0

if [ "$TAURI_VERSION" != "$TAG_VERSION" ]; then
  echo "FAIL: tag says $TAG_VERSION, $TAURI_CONF says ${TAURI_VERSION:-<unset>}"
  status=1
fi

if [ "$CARGO_VERSION" != "$TAG_VERSION" ]; then
  echo "FAIL: tag says $TAG_VERSION, $CARGO_TOML says ${CARGO_VERSION:-<unset>}"
  status=1
fi

if [ "$status" -ne 0 ]; then
  echo
  echo "      Bump both manifests to $TAG_VERSION and retag, or tag the version"
  echo "      the manifests actually declare."
  exit "$status"
fi

echo "OK: $TAG, tauri.conf.json and Cargo.toml all say $TAG_VERSION"
