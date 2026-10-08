"""Plain MAJOR.MINOR.PATCH versions. Prereleases are out of scope."""
from __future__ import annotations

import re

_PLAIN = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")

BUMPS = ("none", "patch", "minor", "major")


def parse(version: str) -> tuple:
    m = _PLAIN.match(version)
    if not m:
        raise ValueError(f"not a plain MAJOR.MINOR.PATCH version: {version!r}")
    return tuple(int(part) for part in m.groups())


def bump(version: str, kind: str) -> str:
    major, minor, patch = parse(version)
    if kind == "none":
        return version
    if kind == "patch":
        return f"{major}.{minor}.{patch + 1}"
    if kind == "minor":
        return f"{major}.{minor + 1}.0"
    if kind == "major":
        return f"{major + 1}.0.0"
    raise ValueError(f"unknown bump kind: {kind!r}")
