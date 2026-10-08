"""PR labels: `semver:<label>:<none|patch|minor|major>`, one per component."""
from __future__ import annotations

from . import semver
from .paths import touches

PREFIX = "semver:"

# Renovate labels its PRs by package rule, not by anyone who can judge what
# changed, so it is exempt from the "code changed, so not `none`" rule.
RENOVATE_LOGINS = frozenset({"renovate[bot]"})


def parse_labels(labels, registry):
    """({component name: bump}, [errors]) from the semver:* labels."""
    by_label = {c.label: c for c in registry.values()}
    chosen = {}
    errors = []
    for label in labels:
        if not label.startswith(PREFIX):
            continue
        parts = label.split(":")
        if len(parts) != 3:
            errors.append(f"'{label}' is malformed; expected semver:<component>:<bump>")
            continue
        _, comp_label, bump = parts
        comp = by_label.get(comp_label)
        if comp is None:
            known = ", ".join(sorted(by_label)) or "none registered"
            errors.append(f"'{label}' names an unknown component (known: {known})")
            continue
        if bump not in semver.BUMPS:
            errors.append(f"'{label}' has an unknown bump; use one of {', '.join(semver.BUMPS)}")
            continue
        if comp.name in chosen:
            errors.append(f"{comp.name} has more than one semver label; keep exactly one")
            continue
        chosen[comp.name] = bump
    return chosen, errors


def check_labels(registry, labels, changed_files, is_renovate):
    """(chosen bumps, errors) for a PR's labels against what it changed."""
    chosen, errors = parse_labels(labels, registry)
    if not chosen and not errors:
        names = ", ".join(f"semver:{c.label}:<bump>" for c in registry.values())
        errors.append(f"no semver label; add one of {names} (use none for changes that ship nothing)")
    for comp in registry.values():
        changed = touches(comp.code_paths, comp.ignore_paths, changed_files)
        bump = chosen.get(comp.name)
        if changed and bump is None and chosen:
            errors.append(f"{comp.name} code changed but the PR has no semver:{comp.label}:* label")
        if changed and bump == "none" and not is_renovate:
            errors.append(
                f"{comp.name} code changed, so semver:{comp.label}:none is not allowed; "
                f"use patch, minor or major"
            )
        if not changed and bump not in (None, "none"):
            errors.append(
                f"no {comp.name} code changed, so semver:{comp.label}:{bump} would release "
                f"nothing; use semver:{comp.label}:none"
            )
    return chosen, errors


def expected_versions(registry, base_versions, chosen):
    """{component: version version.json must hold after this PR}."""
    out = {}
    for name in registry:
        if name not in base_versions:
            raise KeyError(f"version.json on the base branch has no entry for {name}")
        out[name] = semver.bump(base_versions[name], chosen.get(name, "none"))
    return out
