"""The component registry (.github/release-components.json) and version.json."""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from . import semver

REGISTRY_PATH = ".github/release-components.json"
VERSIONS_PATH = "version.json"


class RegistryError(Exception):
    pass


@dataclass(frozen=True)
class Component:
    name: str
    tag: str
    label: str
    synced: tuple  # of (kind, path)
    code_paths: tuple
    ignore_paths: tuple

    def tag_for(self, version: str) -> str:
        return self.tag.replace("{version}", version)

    def version_from_tag(self, tag: str) -> Optional[str]:
        """The version a tag of this component names, or None if it is not one."""
        prefix, _, suffix = self.tag.partition("{version}")
        if not tag.startswith(prefix) or not tag.endswith(suffix):
            return None
        core = tag[len(prefix): len(tag) - len(suffix)]
        try:
            semver.parse(core)
        except ValueError:
            return None
        return core


def safe_path(root, rel) -> Path:
    """root/rel, refusing symlinks and anything that resolves outside root.

    The single guard for every head-checkout path the tool reads or writes: a
    PR controls those files and must not aim a write at anything else.
    """
    root = Path(root)
    path = root / rel
    if path.is_symlink():
        raise RegistryError(f"{rel}: refusing to follow a symlink")
    try:
        path.resolve().relative_to(root.resolve())
    except ValueError:
        raise RegistryError(f"{rel}: resolves outside {root}")
    return path


def load_registry(path) -> dict:
    try:
        raw = json.loads(Path(path).read_text())
    except (OSError, ValueError) as e:
        raise RegistryError(f"cannot read registry {path}: {e}")
    registry = {}
    labels = set()
    for name, entry in raw.items():
        try:
            tag = entry["tag"]
            label = entry["label"]
            synced = tuple((s["kind"], s["path"]) for s in entry["synced"])
            code = tuple(entry["code_paths"])
            ignore = tuple(entry.get("ignore_paths", []))
        except (KeyError, TypeError) as e:
            raise RegistryError(f"component {name!r} is missing {e}")
        if tag.count("{version}") != 1:
            raise RegistryError(f"component {name!r}: tag must contain {{version}} exactly once")
        if label in labels:
            raise RegistryError(f"component {name!r}: label {label!r} is used twice")
        labels.add(label)
        registry[name] = Component(name, tag, label, synced, code, ignore)
    return registry


def read_versions(root) -> dict:
    path = safe_path(root, VERSIONS_PATH)
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError) as e:
        raise RegistryError(f"cannot read {path}: {e}")
    for name, version in data.items():
        try:
            semver.parse(version)
        except (ValueError, TypeError):
            raise RegistryError(f"{path}: {name} has an invalid version {version!r}")
    return data


def write_versions(root, versions: dict) -> None:
    text = json.dumps(versions, indent=2, sort_keys=True) + "\n"
    safe_path(root, VERSIONS_PATH).write_text(text)
