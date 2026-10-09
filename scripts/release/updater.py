"""The Tauri updater feed (`latest.json`) for one release, built from its assets.

Installed apps fetch `/releases/latest/download/latest.json`, so this file is
what every install trusts to say which installer is newest and how it was
signed. Pure: the GitHub calls live in `cli.cmd_updater_manifest`.
"""
from __future__ import annotations

from urllib.parse import quote

from . import semver

# platform -> (installer asset suffix, the updater keys it serves). The plugin
# looks up "<os>-<arch>-<installer>" first, so the keys name the installer
# type. One universal macOS bundle serves both Mac architectures.
PLATFORMS = {
    "windows": (".msi", ("windows-x86_64-msi",)),
    "linux": (".deb", ("linux-x86_64-deb",)),
    "macos": (".app.tar.gz", ("darwin-aarch64", "darwin-x86_64")),
}


class ManifestError(ValueError):
    pass


def build_manifest(*, version, notes, pub_date, tag, repo, asset_names, signatures, require) -> dict:
    """`signatures` maps a `.sig` asset's name to its contents."""
    semver.parse(version)
    platforms = {}
    for platform, (suffix, keys) in PLATFORMS.items():
        matches = sorted(n for n in asset_names if n.endswith(suffix))
        if not matches:
            if platform in require:
                raise ManifestError(f"no {platform} installer (*{suffix}) on the release")
            continue
        if len(matches) > 1:
            raise ManifestError(f"more than one {platform} installer: {matches}")
        name = matches[0]
        signature = signatures.get(name + ".sig")
        if signature is None:
            raise ManifestError(
                f"{name} has no {name}.sig; one unsigned entry makes every client reject the update"
            )
        # From the tag, not the asset's browser_download_url: a draft's URL
        # says `untagged-…` and stops working once the release is published.
        url = f"https://github.com/{repo}/releases/download/{quote(tag)}/{quote(name)}"
        for key in keys:
            platforms[key] = {"url": url, "signature": signature.strip()}
    return {"version": version, "notes": notes, "pub_date": pub_date, "platforms": platforms}
