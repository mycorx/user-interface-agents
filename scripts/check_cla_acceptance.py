#!/usr/bin/env python3
"""Gates a pull request on CLA acceptance.

A contributor accepts the CLA (CLA.md) once, by putting a trailer on a commit:

    MycorX-CLA-Accepted: v1.0

After that they are recorded in CONTRIBUTOR_LEDGER.md by the post-merge job,
and never need the trailer again. This script is what enforces the "once".

Why the gate reads the pull request AUTHOR and not commit authorship
--------------------------------------------------------------------
Commit author fields are unauthenticated. Anyone can run

    git commit --author="Someone Else <their@address>"

and GitHub will attribute that commit to whichever account owns the address.
Gating on commit authorship would therefore let a stranger inherit a ledgered
contributor's acceptance simply by typing their email. `pull_request.user.login`
is the account that actually opened the pull request, which is authenticated,
so that is what decides pass or fail.

Commit authors are still examined, but only to WARN: a pull request carrying
work from more than one person is a case where the maintainer may need a second
acceptance, and that judgement is not one this script can make safely.

Both JSON inputs are file arguments rather than being read from the
environment, so the whole thing runs against fixtures in tests with no network
and no GitHub. In CI the workflow writes $GITHUB_EVENT_PATH and the output of
`gh api .../pulls/N/commits` into files and passes those paths.

Usage:
    check_cla_acceptance.py <event.json> <commits.json> <ledger.md>
"""

import json
import re
import sys
from pathlib import Path

CLA_VERSION = "v1.0"
TRAILER_KEY = "MycorX-CLA-Accepted"

# Git trailers are conventionally `Key: value` at the start of a line. The key
# is matched case-insensitively (contributors type it by hand) but the version
# is matched exactly -- accepting "v0.9" against a v1.0 agreement would record
# consent to terms the contributor never read.
TRAILER_RE = re.compile(
    rf"^\s*{re.escape(TRAILER_KEY)}\s*:\s*{re.escape(CLA_VERSION)}\s*$",
    re.IGNORECASE | re.MULTILINE,
)


def load_json(path: str):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        sys.exit(f"FAIL: could not read {path}: {exc}")


def ledger_logins(path: str) -> set[str]:
    """Logins recorded in the ledger's markdown table, lowercased.

    The table's first column holds `@login`. Anything else on the line is
    ignored, so the file stays free to grow columns without breaking this.
    """
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError:
        # A missing ledger is not an error: before the first outside
        # contributor there is nothing to record, and every author is new.
        return set()
    return {m.lower() for m in re.findall(r"^\s*\|\s*@([A-Za-z0-9-]+)\s*\|", text, re.MULTILINE)}


def is_bot(login: str) -> bool:
    return login.endswith("[bot]")


def main() -> int:
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    event, commits, ledger_path = (load_json(sys.argv[1]), load_json(sys.argv[2]), sys.argv[3])

    pr = event.get("pull_request") or {}
    author = ((pr.get("user") or {}).get("login") or "").strip()
    if not author:
        # Fail closed. A payload without an author is a payload we cannot
        # reason about, and passing it would be the one bug that matters.
        print("FAIL: the event payload names no pull request author")
        return 1

    ledgered = ledger_logins(ledger_path)

    # Advisory only -- see the module docstring on why commit authorship is not
    # trusted for the gate itself. `author` is null when a commit's email
    # matches no GitHub account, which is exactly the spoofing case.
    commit_authors = {
        (c.get("author") or {}).get("login")
        for c in commits
        if (c.get("author") or {}).get("login")
    }
    others = {a for a in commit_authors if a.lower() != author.lower() and not is_bot(a)}
    if others:
        unrecorded = sorted(a for a in others if a.lower() not in ledgered)
        print(
            f"WARNING: this pull request carries commits from more than one author: "
            f"{', '.join(sorted(others))}"
        )
        if unrecorded:
            print(
                f"         not in the ledger: {', '.join(unrecorded)} — if their work is "
                f"included here, they may need to accept the CLA too"
            )

    if is_bot(author):
        print(f"OK: {author} is a bot account, which does not contribute copyrightable work")
        return 0

    if author.lower() in ledgered:
        print(f"OK: {author} accepted the CLA previously and is recorded in the ledger")
        return 0

    if any(TRAILER_RE.search((c.get("commit") or {}).get("message") or "") for c in commits):
        print(f"OK: {author} accepted the CLA {CLA_VERSION} by commit trailer")
        return 0

    print(f"FAIL: {author} has not accepted the CLA (CLA.md).")
    print()
    print("      Add this trailer to a commit in this pull request:")
    print()
    print(f"          {TRAILER_KEY}: {CLA_VERSION}")
    print()
    print("      For example:")
    print()
    print(f'          git commit --amend --trailer "{TRAILER_KEY}: {CLA_VERSION}"')
    print("          git push --force-with-lease")
    print()
    print("      You keep the copyright in your contribution; the CLA grants MycorX")
    print("      a license to use it. This is asked once, not on every pull request.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
