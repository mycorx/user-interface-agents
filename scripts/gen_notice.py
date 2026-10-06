#!/usr/bin/env python3
"""Renders and verifies NOTICE.txt against `cargo metadata` / `pnpm licenses list`.

Invoked by scripts/gen-notice.sh; not meant to be run by hand.

    gen_notice.py render <cargo.json> <npm.json>
    gen_notice.py merge  <cargo.json> <npm.json> <NOTICE.txt>
    gen_notice.py check  <cargo.json> <npm.json> <NOTICE.txt>

`check` asks whether every dependency in the graph is attributed, NOT whether
the file is byte-identical to what `render` would produce today. Four npm
packages are platform-specific (`@esbuild/linux-x64`, `lightningcss-linux-x64-gnu`
and friends), so an exact diff would fail on any machine whose OS or CPU differs
from wherever NOTICE.txt was last generated. A package missing from NOTICE.txt
is an attribution failure; a package listed there but not installed here is
almost certainly another platform's binary, so it is reported as a note rather
than an error. Over-attributing costs nothing.

`merge` exists because `render` alone cannot maintain that union. NOTICE.txt is
meant to hold every platform's packages, but `pnpm licenses list` only ever sees
the store this machine installed — so rendering on Linux writes the win32
binaries out, rendering on Windows writes the linux ones out, and `check` waves
either result through as "expected". No single machine can render the npm half
correctly. `merge` reads the existing file back and keeps what only another
platform can see, so regenerating is additive wherever it is run.

The two halves are merged differently on purpose. `cargo metadata` runs without
`--filter-platform`, so the crate half is already the union on every machine and
is rewritten outright — merging it would only let deleted crates rot in the file
forever. Only the npm half is carried forward.
"""

import json
import re
import sys
from collections import defaultdict

# Crates that publish no `license` field but ship a license file. Each entry is
# read from the crate's own COPYING/LICENSE and must be re-checked whenever the
# crate is bumped — gen-notice.sh fails loudly if a new unknown appears.
LICENSE_OVERRIDES = {
    "webrtc-audio-processing": ("BSD-3-Clause", "Copyright (c) 2011, Google Inc."),
    "webrtc-audio-processing-config": ("BSD-3-Clause", "Copyright (c) 2011, Google Inc."),
    "webrtc-audio-processing-sys": ("BSD-3-Clause", "Copyright (c) 2011, Google Inc."),
}

HEADER = """\
NOTICE — Third-Party Software

This file lists the third-party software distributed with or built into
U.I.A. (User Interface Agent).

UIA's own source code is licensed under the PolyForm Internal Use License
1.0.0 or the PolyForm Noncommercial License 1.0.0, at your choice
(LICENSE.md), and its names, logos, and icons are covered by TRADEMARKS.md.
Neither applies to the packages below: each one remains under its own license,
held by its own copyright holders, and those licenses govern that code whatever
UIA's license says.

Generated from the dependency graph by scripts/gen-notice.sh. Re-run it after
any dependency change.
"""

FOOTER = """\
Obtaining license texts
-----------------------

The full text of each license above ships inside the corresponding package:

  - Rust crates:   ~/.cargo/registry/src/*/<crate>-<version>/
  - npm packages:  node_modules/<package>/

Standard license texts are also available at https://spdx.org/licenses/.

Notes on specific licenses
--------------------------

  MPL-2.0 — file-level copyleft. Modifications to an MPL-2.0 file must be
  released under MPL-2.0; merely linking the unmodified package does not
  affect UIA's own license. UIA does not modify any MPL-2.0 dependency.

  Unicode-3.0 / CDLA-Permissive-2.0 — permissive data licenses covering
  bundled Unicode tables and root-certificate data, not code.

  "A OR B" — a dual license; the recipient chooses either. UIA does not
  elect one on your behalf.
"""


CRATES_TITLE = "Rust crates"
NPM_TITLE = "npm packages"

# `assemble` writes FOOTER last, and its first line is where the entry tables
# stop. Parsing past it would read the license notes as if they were packages.
FOOTER_START = FOOTER.splitlines()[0]


def section(title, rows):
    """Renders one license group: a heading, then `name version` lines."""
    lines = [title, "-" * len(title), ""]
    width = max(len(name) for name, _ in rows)
    for name, version in sorted(rows):
        lines.append(f"  {name.ljust(width)}  {version}")
    lines.append("")
    return "\n".join(lines)


def cargo_groups(path):
    meta = json.load(open(path))
    workspace = set(meta["workspace_members"])
    groups = defaultdict(list)
    unknown = []
    for pkg in meta["packages"]:
        if pkg["id"] in workspace:
            continue
        name = pkg["name"]
        license_ = pkg.get("license")
        if not license_:
            override = LICENSE_OVERRIDES.get(name)
            if override is None:
                unknown.append(f"{name} {pkg['version']}")
                continue
            license_ = override[0]
        groups[license_].append((name, pkg["version"]))
    if unknown:
        raise SystemExit(
            "gen-notice: no license for "
            + ", ".join(unknown)
            + "\nRead the crate's LICENSE/COPYING file and add it to "
            "LICENSE_OVERRIDES in scripts/gen_notice.py."
        )
    return groups


def npm_groups(path):
    groups = defaultdict(list)
    for license_, pkgs in json.load(open(path)).items():
        for pkg in pkgs:
            versions = pkg.get("versions") or [pkg.get("version", "")]
            for version in versions:
                groups[license_].append((pkg["name"], version))
    return groups


def render(title, groups):
    """Orders groups by size so the long permissive blocks come first."""
    out = [title, "=" * len(title), ""]
    if not groups:
        out.append("  (none)\n")
        return "\n".join(out)
    total = sum(len(v) for v in groups.values())
    out.append(f"{total} packages, grouped by declared SPDX license expression.\n")
    for license_, rows in sorted(groups.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        out.append(section(license_, rows))
    return "\n".join(out)


def assemble(crates, npm):
    """The whole file, from the two halves' (license -> rows) groups."""
    return "\n".join([
        HEADER,
        "",
        render(CRATES_TITLE, crates),
        "",
        render(NPM_TITLE, npm),
        "",
        FOOTER,
    ])


# A rendered entry line: two leading spaces, a name, padding, then a version.
ENTRY = re.compile(r"^  (\S+)\s+(\S+)$")


def parse_document(notice_text):
    """Reads a rendered NOTICE.txt back into {half title: {license: [rows]}}.

    The file is its own record: nothing else remembers what another platform
    contributed, so the union can only be recovered by reading what is already
    written. Headings are found by their underline, which is what `section` and
    `render` emit -- `=` under a half, `-` under a license.
    """
    halves = {}
    half = license_ = None
    lines = notice_text.splitlines()
    for i, line in enumerate(lines):
        if line == FOOTER_START:
            break
        underline = lines[i + 1] if i + 1 < len(lines) else ""
        if line and len(underline) == len(line) and set(underline) == {"="}:
            half, license_ = line, None
            halves.setdefault(half, defaultdict(list))
        elif line and len(underline) == len(line) and set(underline) == {"-"} and half:
            license_ = line
        elif half and license_:
            row = ENTRY.match(line)
            if row:
                halves[half][license_].append(row.groups())
    return halves


def carry_forward(fresh, previous):
    """Adds back the npm packages this machine has no way of seeing.

    Carried by NAME, never by (name, version). A package absent from the local
    store is another platform's binary and must survive; a package that IS
    installed here is authoritative, versions included. Carrying pairs instead
    would leave every superseded version in the file forever, so a bump would
    attribute both 5.6.0 and 5.7.0 until someone noticed.

    Returns the merged groups and the rows it carried, because a genuine
    removal is indistinguishable from another platform's binary -- the caller
    has to be able to say out loud what it kept.
    """
    installed = {name for rows in fresh.values() for name, _ in rows}
    merged = defaultdict(list, {license_: list(rows) for license_, rows in fresh.items()})
    carried = []
    for license_, rows in previous.items():
        for name, version in rows:
            if name not in installed:
                merged[license_].append((name, version))
                carried.append((name, version))
    return merged, sorted(set(carried))


def merge(cargo_path, npm_path, notice_path):
    try:
        previous = parse_document(open(notice_path).read())
    except FileNotFoundError:
        previous = {}  # bootstrap: no file yet, so there is nothing to carry.

    npm, carried = carry_forward(npm_groups(npm_path), previous.get(NPM_TITLE, {}))
    if carried:
        print(f"note: kept {len(carried)} npm package(s) that are not installed here "
              "(other platforms' binaries):", file=sys.stderr)
        for name, version in carried:
            print(f"  {name} {version}", file=sys.stderr)
        print("If one of these was deliberately removed, re-run with --rewrite.",
              file=sys.stderr)
    sys.stdout.write(assemble(cargo_groups(cargo_path), npm))


def attributed(notice_text):
    """Every (name, version) pair NOTICE.txt currently attributes."""
    return {m.groups() for m in map(ENTRY.match, notice_text.splitlines()) if m}


def check(cargo_path, npm_path, notice_path):
    try:
        listed = attributed(open(notice_path).read())
    except FileNotFoundError:
        raise SystemExit(f"gen-notice: {notice_path} does not exist — run ./scripts/gen-notice.sh")

    required = set()
    for groups in (cargo_groups(cargo_path), npm_groups(npm_path)):
        for rows in groups.values():
            required.update(rows)

    missing = sorted(required - listed)
    if missing:
        print(f"FAIL: {len(missing)} dependency/ies are not attributed in {notice_path}:", file=sys.stderr)
        for name, version in missing:
            print(f"  {name} {version}", file=sys.stderr)
        print("\nRun ./scripts/gen-notice.sh and commit the result.", file=sys.stderr)
        raise SystemExit(1)

    extra = sorted(listed - required)
    if extra:
        print(f"note: {len(extra)} entry/ies listed but not installed here "
              "(expected — other platforms' binaries):")
        for name, version in extra:
            print(f"  {name} {version}")
    print(f"OK: all {len(required)} dependencies of this platform are attributed in {notice_path}")


def main():
    mode = sys.argv[1]
    if mode == "render":
        sys.stdout.write(assemble(cargo_groups(sys.argv[2]), npm_groups(sys.argv[3])))
    elif mode == "merge":
        merge(sys.argv[2], sys.argv[3], sys.argv[4])
    elif mode == "check":
        check(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        raise SystemExit(f"gen-notice: unknown mode {mode!r}")


if __name__ == "__main__":
    main()
