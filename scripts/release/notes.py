"""Release notes, built from the Release note section of each merged PR."""
from __future__ import annotations

from . import labels as labels_mod
from . import prbody

_HEADINGS = (
    ("major", "Breaking changes"),
    ("minor", "Features"),
    ("patch", "Fixes"),
)


def _bump_for(pr: dict, comp):
    for label in pr.get("labels", []):
        if label.startswith(f"{labels_mod.PREFIX}{comp.label}:"):
            return label.split(":")[2]
    return None


def build_notes(prs: list, comp) -> str:
    """prs: [{number, title, url, author, labels, body}], merged since the baseline."""
    seen = set()
    groups = {"major": [], "minor": [], "patch": []}
    dependencies = []
    for pr in sorted(prs, key=lambda p: p["number"]):
        if pr["number"] in seen:
            continue
        seen.add(pr["number"])
        link = f"[#{pr['number']}]({pr['url']})"
        if pr.get("author") in labels_mod.RENOVATE_LOGINS:
            dependencies.append(f"- {pr['title']} ({link})")
            continue
        bump = _bump_for(pr, comp)
        note = prbody.release_note(pr.get("body") or "")
        if bump in groups and note:
            groups[bump].append(f"- {note} ({link})")

    sections = []
    for kind, heading in _HEADINGS:
        if groups[kind]:
            sections.append(f"## {heading}\n\n" + "\n".join(groups[kind]))
    if dependencies:
        sections.append("## Dependencies\n\n" + "\n".join(dependencies))
    return "\n\n".join(sections) if sections else "No user-facing changes."
