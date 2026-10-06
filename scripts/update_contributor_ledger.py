#!/usr/bin/env python3
"""Records a merged pull request's author in CONTRIBUTOR_LEDGER.md.

Runs after a pull request merges. Once an author is in the ledger they never
need the CLA trailer again, which is the whole point of keeping the file: the
gate (check_cla_acceptance.py) asks for acceptance once, not every time.

Why the ledger, rather than trusting the pull request body
----------------------------------------------------------
A pull request description is editable by its author after merge, and GitHub
lets the author delete the content of prior revisions from the edit history.
A commit trailer merged into `main` cannot be quietly altered, and this file
turns the scattered trailers into one reviewable list.

Why it reads acceptance from the pull request's commits rather than from `main`
-------------------------------------------------------------------------------
Squash merging composes a new commit message that the merger can edit, so the
trailer may not survive into `main` at all. The pull request's own commit list
still has it, and that list does not change after the merge.

Writing nothing is always safe here: the gate already ran before the merge.
This script never fabricates a record it cannot evidence.

Usage:
    update_contributor_ledger.py <event.json> <commits.json> <ledger.md>
"""

import datetime as dt
import json
import re
import sys
from pathlib import Path

CLA_VERSION = "v1.0"
TRAILER_KEY = "MycorX-CLA-Accepted"
TRAILER_RE = re.compile(
    rf"^\s*{re.escape(TRAILER_KEY)}\s*:\s*(v[0-9]+\.[0-9]+)\s*$",
    re.IGNORECASE | re.MULTILINE,
)


def load_json(path: str):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        sys.exit(f"FAIL: could not read {path}: {exc}")


def main() -> int:
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    event, commits, ledger_path = (load_json(sys.argv[1]), load_json(sys.argv[2]), sys.argv[3])

    pr = event.get("pull_request") or {}
    if not pr.get("merged"):
        print("nothing to do: this pull request was closed without merging")
        return 0

    author = ((pr.get("user") or {}).get("login") or "").strip()
    if not author:
        print("nothing to do: the event payload names no pull request author")
        return 0
    if author.endswith("[bot]"):
        print(f"nothing to do: {author} is a bot account")
        return 0

    ledger = Path(ledger_path)
    text = ledger.read_text(encoding="utf-8") if ledger.exists() else ""
    existing = {m.lower() for m in re.findall(r"^\s*\|\s*@([A-Za-z0-9-]+)\s*\|", text, re.MULTILINE)}
    # Idempotent by design: a replayed workflow run must not append a
    # second row for someone already recorded.
    if author.lower() in existing:
        print(f"nothing to do: {author} is already in the ledger")
        return 0

    version = None
    for commit in commits:
        match = TRAILER_RE.search((commit.get("commit") or {}).get("message") or "")
        if match:
            version = match.group(1)
            break
    if not version:
        # The gate should have prevented this, so say so loudly rather than
        # inventing an acceptance that no commit evidences.
        print(f"WARNING: {author} merged without a CLA trailer and was NOT added to the ledger")
        print("         (check how this pull request passed the cla job)")
        return 0

    # The display name is a courtesy for humans reading the file. It comes from
    # the commit the trailer sits on, and falls back to the login.
    name = author
    for commit in commits:
        if ((commit.get("author") or {}).get("login") or "").lower() == author.lower():
            name = ((commit.get("commit") or {}).get("author") or {}).get("name") or author
            break

    merged_at = pr.get("merged_at") or ""
    date = merged_at[:10] or dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d")
    row = f"| @{author} | {name} | {version} | #{pr.get('number', '?')} | {date} |"

    lines = text.splitlines()
    # Append directly beneath the last table row, so anything written after the
    # table (notes, a footer) stays where its author put it.
    last_row = max((i for i, line in enumerate(lines) if line.lstrip().startswith("|")), default=-1)
    if last_row == -1:
        sys.exit(f"FAIL: {ledger_path} has no markdown table to append to")
    lines.insert(last_row + 1, row)
    ledger.write_text("\n".join(lines) + "\n", encoding="utf-8")

    print(f"recorded {author} in {ledger_path} (CLA {version}, #{pr.get('number', '?')})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
