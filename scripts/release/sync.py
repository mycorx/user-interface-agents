"""Writes (and verifies) a component's version into the files that carry it.

Edits are line-level, never parse-and-dump, so a bump changes one line and
leaves the file's formatting alone. Each write is re-read to prove it landed.
Python 3.9 has no tomllib, and the two TOML shapes involved are simple.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

from .registry import Component


class SyncError(Exception):
    pass


_TABLE = re.compile(r"^\[(?!\[)([^\[\]]+)\]\s*(?:#.*)?$")
_ARRAY_TABLE = re.compile(r"^\[\[([^\[\]]+)\]\]\s*(?:#.*)?$")
_KEY = re.compile(r'^(version|name)\s*=\s*"([^"]*)"\s*(?:#.*)?$')


# --- tauri.conf.json -------------------------------------------------------

_TAURI_VERSION = re.compile(r'(?m)^(  "version":\s*")([^"]*)(")')


def _tauri_read(text: str) -> str:
    return json.loads(text).get("version", "")


def _tauri_write(text: str, version: str) -> str:
    # Top-level keys sit at two spaces; the nested `version` keys a tauri
    # config also carries sit deeper, so the indent is the discriminator.
    new, n = _TAURI_VERSION.subn(lambda m: m.group(1) + version + m.group(3), text, count=1)
    if n != 1:
        raise SyncError('no top-level `  "version": "..."` line (two-space indent) found')
    if _tauri_read(new) != version:
        raise SyncError("tauri.conf.json version did not take the edit")
    return new


# --- Cargo.toml ------------------------------------------------------------


def _scan_toml_package(text: str):
    """(name, version, version_line_index) of the [package] table."""
    name = version = None
    index = -1
    section = None
    for i, line in enumerate(text.split("\n")):
        s = line.strip()
        if s.startswith("["):
            m = _TABLE.match(s)
            section = m.group(1).strip() if m else "<array>"
            continue
        if section != "package":
            continue
        m = _KEY.match(s)
        if m and m.group(1) == "name":
            name = m.group(2)
        elif m and m.group(1) == "version":
            version, index = m.group(2), i
    return name, version, index


def _cargo_read(text: str) -> str:
    _, version, _ = _scan_toml_package(text)
    if version is None:
        raise SyncError("[package] has no literal `version = \"...\"` line")
    return version


def _cargo_write(text: str, version: str) -> str:
    _, current, index = _scan_toml_package(text)
    if current is None:
        raise SyncError("[package] has no literal `version = \"...\"` line")
    lines = text.split("\n")
    lines[index] = re.sub(r'(version\s*=\s*")[^"]*(")', lambda m: m.group(1) + version + m.group(2), lines[index], count=1)
    new = "\n".join(lines)
    if _cargo_read(new) != version:
        raise SyncError("Cargo.toml version did not take the edit")
    return new


def _package_name(text: str) -> str:
    name, _, _ = _scan_toml_package(text)
    if not name:
        raise SyncError("[package] has no `name`")
    return name


# --- Cargo.lock ------------------------------------------------------------


def _lock_index(text: str, package: str) -> int:
    """Line index of `version = ...` inside the one [[package]] named `package`."""
    found = []
    in_package = False
    name = None
    for i, line in enumerate(text.split("\n")):
        s = line.strip()
        if s.startswith("["):
            in_package = bool(_ARRAY_TABLE.match(s)) and _ARRAY_TABLE.match(s).group(1) == "package"
            name = None
            continue
        if not in_package:
            continue
        m = _KEY.match(s)
        if m and m.group(1) == "name":
            name = m.group(2)
        elif m and m.group(1) == "version" and name == package:
            found.append(i)
    if len(found) != 1:
        raise SyncError(f"expected exactly one [[package]] named {package!r} in Cargo.lock, found {len(found)}")
    return found[0]


def _lock_read(text: str, package: str) -> str:
    i = _lock_index(text, package)
    return _KEY.match(text.split("\n")[i].strip()).group(2)


def _lock_write(text: str, package: str, version: str) -> str:
    i = _lock_index(text, package)
    lines = text.split("\n")
    lines[i] = re.sub(r'(version\s*=\s*")[^"]*(")', lambda m: m.group(1) + version + m.group(2), lines[i], count=1)
    new = "\n".join(lines)
    if _lock_read(new, package) != version:
        raise SyncError("Cargo.lock version did not take the edit")
    return new


# --- public ----------------------------------------------------------------


def _package_for_lock(root: Path, comp: Component) -> str:
    for kind, path in comp.synced:
        if kind == "cargo-toml":
            return _package_name((root / path).read_text())
    raise SyncError(f"{comp.name}: a cargo-lock entry needs a cargo-toml entry beside it")


def read_synced(root, comp: Component) -> list:
    """[(path, version)] as the files say it now."""
    root = Path(root)
    out = []
    for kind, path in comp.synced:
        try:
            text = (root / path).read_text()
            if kind == "tauri-conf":
                out.append((path, _tauri_read(text)))
            elif kind == "cargo-toml":
                out.append((path, _cargo_read(text)))
            elif kind == "cargo-lock":
                out.append((path, _lock_read(text, _package_for_lock(root, comp))))
            else:
                raise SyncError(f"unknown synced kind {kind!r}")
        except (OSError, ValueError) as e:
            raise SyncError(f"{path}: {e}")
        except SyncError as e:
            raise SyncError(f"{path}: {e}")
    return out


def apply(root, comp: Component, version: str) -> None:
    root = Path(root)
    for kind, path in comp.synced:
        file = root / path
        try:
            text = file.read_text()
            if kind == "tauri-conf":
                new = _tauri_write(text, version)
            elif kind == "cargo-toml":
                new = _cargo_write(text, version)
            elif kind == "cargo-lock":
                new = _lock_write(text, _package_for_lock(root, comp), version)
            else:
                raise SyncError(f"unknown synced kind {kind!r}")
        except OSError as e:
            raise SyncError(f"{path}: {e}")
        except SyncError as e:
            raise SyncError(f"{path}: {e}")
        if new != text:
            file.write_text(new)


def check(root, comp: Component, version: str) -> list:
    """Error messages, one per synced file that does not say `version`."""
    try:
        current = read_synced(root, comp)
    except SyncError as e:
        return [str(e)]
    return [
        f"{path} says {got}, but version.json says {version} for {comp.name}"
        for path, got in current
        if got != version
    ]
