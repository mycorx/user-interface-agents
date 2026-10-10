"""Which tag a manual release promotes, and which tags a merge should create."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Optional

from . import semver


class PlanError(Exception):
    pass


class TagError(Exception):
    pass


@dataclass(frozen=True)
class Plan:
    component: str
    baseline: Optional[str]
    target: str
    target_version: str
    target_sha: str
    candidates: tuple


def make_plan(comp, tags: dict, published: set, is_ancestor: Callable, on_main: Callable) -> Plan:
    """tags: {tag name: commit sha}; published: tag names of published releases."""
    versions = {t: comp.version_from_tag(t) for t in tags}
    versions = {t: v for t, v in versions.items() if v is not None}

    shipped = [t for t in published if comp.version_from_tag(t) is not None]
    baseline = max(shipped, key=lambda t: semver.parse(comp.version_from_tag(t))) if shipped else None
    floor = semver.parse(comp.version_from_tag(baseline)) if baseline else None

    candidates = sorted(
        (t for t, v in versions.items() if floor is None or semver.parse(v) > floor),
        key=lambda t: semver.parse(versions[t]),
    )
    if not candidates:
        since = f"since {baseline}" if baseline else "at all"
        raise PlanError(f"nothing to release: no {comp.name} tag exists {since}")

    target = candidates[-1]
    if baseline is not None and baseline in tags and not is_ancestor(tags[baseline], tags[target]):
        raise PlanError(f"{target} is not a descendant of the published {baseline}")
    if not on_main(tags[target]):
        raise PlanError(f"{target} ({tags[target][:7]}) is not reachable from main")
    return Plan(comp.name, baseline, target, versions[target], tags[target], tuple(candidates))


def make_head_plan(comp, version: str, sha: str) -> Plan:
    """The plan for a dry run from a branch: build that commit, at its own version.

    `make_plan` promotes the newest unreleased tag on main, which would make a
    dry run from a branch build main and prove nothing about the branch. A dry
    run creates no release, so there is no tag, no baseline and no candidates.
    """
    semver.parse(version)
    return Plan(comp.name, None, "", version, sha, ())


def tags_to_create(registry, versions: dict, existing: set) -> list:
    """[(component, tag)] a merge to main must create for the versions in version.json."""
    out = []
    for comp in registry.values():
        version = versions.get(comp.name)
        if version is None:
            raise TagError(f"version.json has no entry for {comp.name}")
        tag = comp.tag_for(version)
        if tag in existing:
            continue
        known = [comp.version_from_tag(t) for t in existing if comp.version_from_tag(t)]
        if not known:
            raise TagError(
                f"{comp.name} has no tags at all, so {tag} would land on this commit by "
                f"accident. Create the prefixed tag for the LAST PUBLISHED release by hand "
                f"(e.g. {comp.tag_for('0.1.1')} on the commit that release was built from), as in "
                f"docs/RELEASE.md 'First-time setup', then re-run."
            )
        newest = max(known, key=semver.parse)
        if semver.parse(version) < semver.parse(newest):
            raise TagError(
                f"version.json says {version} for {comp.name}, older than the existing tag "
                f"for {newest}; versions must only go up"
            )
        out.append((comp.name, tag))
    return out
