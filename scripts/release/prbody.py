"""The PR description check, and the release note the notes are built from."""
from __future__ import annotations

import re

REQUIRED = ("Summary", "Release note", "Testing")
_EMPTY = {"", "tbd", "todo"}
_HEADING = re.compile(r"^#{1,6}\s+(.*?)\s*#*\s*$")


def strip_comments(body: str) -> str:
    return re.sub(r"<!--.*?-->", "", body or "", flags=re.S)


def sections(body: str) -> dict:
    """{lower-cased heading: text beneath it}, comments already stripped."""
    out = {}
    current = None
    buf = []
    for line in strip_comments(body).splitlines():
        m = _HEADING.match(line)
        if m:
            if current is not None:
                out[current] = "\n".join(buf).strip()
            current = m.group(1).strip().lower()
            buf = []
        elif current is not None:
            buf.append(line)
    if current is not None:
        out[current] = "\n".join(buf).strip()
    return out


def check_body(body: str, requires_release_note: bool) -> list:
    found = sections(body)
    errors = []
    for name in REQUIRED:
        text = found.get(name.lower())
        if text is None:
            errors.append(f"the PR description has no '## {name}' section; see the PR template")
        elif text.lower() in _EMPTY:
            errors.append(f"the '{name}' section is empty")
    note = found.get("release note")
    if requires_release_note and note is not None and note.lower() in {"n/a", "na", "none"}:
        errors.append(
            "'Release note' is N/A, but this PR bumps a version; write one user-facing sentence"
        )
    return errors


def release_note(body: str):
    """The Release note text, or None when absent, empty or N/A."""
    note = sections(body).get("release note")
    if note is None or note.lower() in _EMPTY | {"n/a", "na", "none"}:
        return None
    return " ".join(note.split())
