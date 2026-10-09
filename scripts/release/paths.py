"""Glob matching with the `**` semantics fnmatch does not have.

`*` stays inside one path segment, `**/` matches any number of directories
(including none), and a trailing `**` matches everything below.
"""
from __future__ import annotations

import re
from typing import Iterable


def _compile(pattern: str):
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("^" + "".join(out) + "$")


def matches(pattern: str, path: str) -> bool:
    return bool(_compile(pattern).match(path))


def touches(code_paths: Iterable[str], ignore_paths: Iterable[str], files: Iterable[str]) -> bool:
    """True when any file is a code path and not an ignored one."""
    code = list(code_paths)
    ignore = list(ignore_paths)
    for f in files:
        if any(matches(p, f) for p in code) and not any(matches(p, f) for p in ignore):
            return True
    return False
