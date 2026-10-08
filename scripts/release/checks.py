"""Everything `release-check` enforces, as one pure function."""
from __future__ import annotations

from . import labels as labels_mod
from . import prbody
from .labels import RENOVATE_LOGINS

BUMP_HINT = (
    "run `python3 scripts/release bump --root . --base-root <checkout of the base branch> "
    "--label <the PR's semver:... label> --changed-files <file listing the PR's paths>` "
    "and commit the result (see docs/RELEASE.md)"
)


def check_pr(*, registry, labels, changed_files, author, body, base_versions, head_versions, sync_errors):
    is_renovate = author in RENOVATE_LOGINS
    chosen, errors = labels_mod.check_labels(registry, labels, changed_files, is_renovate)
    if not errors:
        try:
            expected = labels_mod.expected_versions(registry, base_versions, chosen)
        except KeyError as e:
            return errors + [str(e.args[0])]
        for name, want in expected.items():
            got = head_versions.get(name)
            if got != want:
                errors.append(
                    f"version.json says {got!r} for {name}, expected {want} "
                    f"({base_versions[name]} + {chosen.get(name, 'none')}); {BUMP_HINT}"
                )
    errors += sync_errors
    if not is_renovate:
        errors += prbody.check_body(
            body, requires_release_note=any(b != "none" for b in chosen.values())
        )
    return errors
