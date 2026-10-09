"""Command line for the release tooling. Run: python3 scripts/release <command>."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from . import checks, labels as labels_mod, notes, plan as plan_mod, sync, updater
from .registry import REGISTRY_PATH, RegistryError, load_registry, read_versions, write_versions


class CommandError(Exception):
    pass


# --- IO helpers (the only impure part; everything they feed is tested) -------


def _run(args, cwd=None) -> str:
    done = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if done.returncode != 0:
        # gh prints the API's explanation (e.g. the ruleset that rejected a
        # tag) as the JSON body on stdout, and only a one-line summary on stderr.
        detail = "\n".join(x for x in (done.stderr.strip(), done.stdout.strip()) if x)
        raise CommandError(f"`{' '.join(args)}` failed: {detail}")
    return done.stdout


def _git(root, *args) -> str:
    return _run(["git", *args], cwd=root)


def _gh_api(path: str, *extra) -> str:
    return _run(["gh", "api", path, *extra])


def _lines(path) -> list:
    return [l for l in Path(path).read_text().splitlines() if l.strip()]


def _out(name: str, value: str) -> None:
    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a") as f:
            f.write(f"{name}={value}\n")


def _summary(markdown: str) -> None:
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a") as f:
            f.write(markdown + "\n")


def _version_of(versions, name):
    if name not in versions:
        raise CommandError(f"version.json has no entry for {name}")
    return versions[name]


def _component(registry, name):
    if name not in registry:
        raise CommandError(f"unknown component {name!r}; registered: {', '.join(registry)}")
    return registry[name]


def _pr_inputs(a):
    """(labels, changed files, author, body) from flags, or from the PR event payload."""
    labels, author = list(a.label), a.author
    body = Path(a.body_file).read_text() if a.body_file else ""
    number = None
    if a.event:
        try:
            payload = json.loads(Path(a.event).read_text())
            if not isinstance(payload, dict) or not isinstance(payload.get("pull_request"), dict):
                raise CommandError("event has no pull_request")
            pr = payload["pull_request"]
            labels = [l["name"] for l in pr.get("labels") or []]
            author = pr["user"]["login"]
            body = pr.get("body") or ""
            number = pr["number"]
        except (KeyError, TypeError, AttributeError) as e:
            raise CommandError(f"event pull_request is malformed ({type(e).__name__}: {e})")
    if a.changed_files:
        files = _lines(a.changed_files)
    elif number is not None:
        # A rename out of a code path is a change to that path, so include previous_filename.
        listing = _gh_api(
            f"repos/{a.repo}/pulls/{number}/files", "--paginate",
            "--jq", '.[] | [.filename, (.previous_filename // "")] | @tsv',
        )
        files = []
        for row in listing.split("\n"):
            files += [p for p in row.split("\t") if p]
    else:
        raise CommandError("give --changed-files, or --event so the files can be fetched")
    return labels, files, author, body


# --- commands ----------------------------------------------------------------


def cmd_check_pr(a) -> int:
    # The registry comes from the BASE checkout: a PR must not be able to
    # redefine what it is judged against. Only data is read from the head.
    registry = load_registry(Path(a.base_root) / REGISTRY_PATH)
    base_versions = read_versions(a.base_root)
    head_versions = read_versions(a.root)
    sync_errors = []
    for comp in registry.values():
        if comp.name in head_versions:
            sync_errors += sync.check(a.root, comp, head_versions[comp.name])
    labels, files, author, body = _pr_inputs(a)
    errors = checks.check_pr(
        registry=registry,
        labels=labels,
        changed_files=files,
        author=author,
        body=body,
        base_versions=base_versions,
        head_versions=head_versions,
        sync_errors=sync_errors,
    )
    if errors:
        print("release-check failed:\n" + "\n".join(f"  - {e}" for e in errors))
        return 1
    print("release-check passed")
    return 0


def cmd_bump(a) -> int:
    registry = load_registry(Path(a.base_root) / REGISTRY_PATH)
    base_versions = read_versions(a.base_root)
    labels, files, author, _ = _pr_inputs(a)
    chosen, errors = labels_mod.check_labels(
        registry, labels, files, author in labels_mod.RENOVATE_LOGINS
    )
    if errors:
        # release-check reports these; failing here too would only double the noise.
        print("not bumping, the labels are not valid yet:\n" + "\n".join(f"  - {e}" for e in errors))
        return 0
    expected = labels_mod.expected_versions(registry, base_versions, chosen)
    head_versions = read_versions(a.root)
    changed = False
    for name, want in expected.items():
        comp = registry[name]
        if head_versions.get(name) != want or sync.check(a.root, comp, want):
            print(f"{name}: {head_versions.get(name)} -> {want}")
            head_versions[name] = want
            # Manifests first (computed whole before writing): if they fail,
            # version.json is left untouched rather than ahead of them.
            sync.apply(a.root, comp, want)
            write_versions(a.root, head_versions)
            changed = True
    if not changed:
        print("version.json and the manifests are already correct")
    return 0


def cmd_sync(a) -> int:
    registry = load_registry(Path(a.root) / REGISTRY_PATH)
    versions = read_versions(a.root)
    names = [a.component] if a.component else list(registry)
    errors = []
    for name in names:
        comp = _component(registry, name)
        have = _version_of(versions, name)
        want = a.expect_version or have
        if a.expect_version and have != want:
            errors.append(f"version.json says {have} for {name}, expected {want}")
        if a.check:
            errors += sync.check(a.root, comp, want)
        else:
            sync.apply(a.root, comp, want)
            print(f"{name}: synced to {want}")
    if errors:
        print("version drift:\n" + "\n".join(f"  - {e}" for e in errors))
        return 1
    if a.check:
        print("version.json and the manifests agree")
    return 0


def cmd_current(a) -> int:
    registry = load_registry(Path(a.root) / REGISTRY_PATH)
    _component(registry, a.component)
    print(_version_of(read_versions(a.root), a.component))
    return 0


def cmd_tag(a) -> int:
    registry = load_registry(Path(a.root) / REGISTRY_PATH)
    versions = read_versions(a.root)
    existing = set(_git(a.root, "tag", "--list").split())
    todo = plan_mod.tags_to_create(registry, versions, existing)
    if not todo:
        print("every version in version.json already has a tag")
    for name, tag in todo:
        if a.dry_run:
            print(f"would tag {a.sha} as {tag}")
            continue
        _gh_api(
            f"repos/{a.repo}/git/refs", "-X", "POST", "-f", f"ref=refs/tags/{tag}", "-f", f"sha={a.sha}"
        )
        print(f"tagged {a.sha} as {tag}")
    return 0


def cmd_plan(a) -> int:
    registry = load_registry(Path(a.root) / REGISTRY_PATH)
    comp = _component(registry, a.component)
    tags = {}
    for tag in _git(a.root, "tag", "--list").split():
        if comp.version_from_tag(tag) is not None:
            tags[tag] = _git(a.root, "rev-list", "-n", "1", tag).strip()
    if a.published_tags_file:
        published = set(_lines(a.published_tags_file))
    else:
        published = set(
            _gh_api(
                f"repos/{a.repo}/releases",
                "--paginate",
                "--jq",
                ".[] | select(.draft == false and .prerelease == false) | .tag_name",
            ).split()
        )

    def is_ancestor(older, newer):
        return subprocess.run(
            ["git", "merge-base", "--is-ancestor", older, newer], cwd=a.root
        ).returncode == 0

    try:
        p = plan_mod.make_plan(comp, tags, published, is_ancestor, lambda sha: is_ancestor(sha, a.main_ref))
    except plan_mod.PlanError as e:
        raise CommandError(str(e))
    print(json.dumps({
        "component": p.component, "baseline": p.baseline, "target": p.target,
        "version": p.target_version, "sha": p.target_sha, "candidates": list(p.candidates),
    }, indent=2))
    _out("target_tag", p.target)
    _out("target_sha", p.target_sha)
    _out("target_version", p.target_version)
    _out("baseline_tag", p.baseline or "")
    _out("candidates", ",".join(p.candidates))
    _summary(
        f"### Release plan: {p.component}\n\n"
        f"- baseline (latest published): `{p.baseline or 'none'}`\n"
        f"- promoting: **`{p.target}`** (`{p.target_sha[:7]}`)\n"
        f"- tags this ships: {', '.join(f'`{t}`' for t in p.candidates)}"
    )
    return 0


def cmd_notes(a) -> int:
    registry = load_registry(Path(a.root) / REGISTRY_PATH)
    comp = _component(registry, a.component)
    span = f"{a.baseline}..{a.target}" if a.baseline else a.target
    prs = {}
    for sha in _git(a.root, "rev-list", span).split():
        found = json.loads(
            _gh_api(
                f"repos/{a.repo}/commits/{sha}/pulls",
                "--jq",
                "[.[] | select(.merged_at != null) | {number, title, url: .html_url, "
                "author: .user.login, labels: [.labels[].name], body}]",
            )
            or "[]"
        )
        for pr in found:
            prs[pr["number"]] = pr
    print(notes.build_notes(list(prs.values()), comp))
    return 0


# --- wiring ------------------------------------------------------------------


def cmd_updater_manifest(a) -> int:
    release = json.loads(_gh_api(f"repos/{a.repo}/releases/{a.release_id}"))
    names = [asset["name"] for asset in release["assets"]]
    signatures = {
        asset["name"]: _gh_api(
            f"repos/{a.repo}/releases/assets/{asset['id']}", "-H", "Accept: application/octet-stream"
        )
        for asset in release["assets"]
        if asset["name"].endswith(".sig")
    }
    manifest = updater.build_manifest(
        version=a.version,
        notes=release.get("body") or "",
        pub_date=datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        tag=a.tag,
        repo=a.repo,
        asset_names=names,
        signatures=signatures,
        require=set(a.require),
    )
    Path(a.out).write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"latest.json: {', '.join(sorted(manifest['platforms']))}")
    return 0


def _parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="release")
    sub = p.add_subparsers(dest="command", required=True)

    def pr_args(s):
        s.add_argument("--root", required=True, help="the PR head checkout")
        s.add_argument("--base-root", required=True, help="the base branch checkout (trusted)")
        s.add_argument("--event", help="the pull_request event payload ($GITHUB_EVENT_PATH)")
        s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
        s.add_argument("--label", action="append", default=[], help="a PR label; repeat")
        s.add_argument("--changed-files", help="file listing the PR's changed paths")
        s.add_argument("--author", default="")
        s.add_argument("--body-file")

    s = sub.add_parser("check-pr")
    pr_args(s)
    s.set_defaults(fn=cmd_check_pr)

    s = sub.add_parser("bump")
    pr_args(s)
    s.set_defaults(fn=cmd_bump)

    s = sub.add_parser("sync")
    s.add_argument("--root", default=".")
    s.add_argument("--component")
    s.add_argument("--check", action="store_true")
    s.add_argument("--expect-version")
    s.set_defaults(fn=cmd_sync)

    s = sub.add_parser("current")
    s.add_argument("--root", default=".")
    s.add_argument("--component", required=True)
    s.set_defaults(fn=cmd_current)

    s = sub.add_parser("tag")
    s.add_argument("--root", default=".")
    s.add_argument("--sha", required=True)
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    s.add_argument("--dry-run", action="store_true")
    s.set_defaults(fn=cmd_tag)

    s = sub.add_parser("plan")
    s.add_argument("--root", default=".")
    s.add_argument("--component", required=True)
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    s.add_argument("--main-ref", default="origin/main")
    s.add_argument("--published-tags-file", help="tests only: skip the GitHub API")
    s.set_defaults(fn=cmd_plan)

    s = sub.add_parser("notes")
    s.add_argument("--root", default=".")
    s.add_argument("--component", required=True)
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    s.add_argument("--baseline", default="")
    s.add_argument("--target", required=True)
    s.set_defaults(fn=cmd_notes)
    s = sub.add_parser("updater-manifest")
    s.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    s.add_argument("--release-id", required=True)
    s.add_argument("--tag", required=True)
    s.add_argument("--version", required=True)
    s.add_argument("--require", action="append", default=[], choices=sorted(updater.PLATFORMS))
    s.add_argument("--out", required=True)
    s.set_defaults(fn=cmd_updater_manifest)
    return p


def main(argv) -> int:
    args = _parser().parse_args(argv)
    try:
        return args.fn(args)
    except (CommandError, RegistryError, sync.SyncError, plan_mod.TagError, ValueError, KeyError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
