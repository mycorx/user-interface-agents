import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from release import cli, updater

REPO = "mycorx/user-interface-agents"
TAG = "uai-app-0.2.0"
MSI = "uia_0.2.0_x64_en-US.msi"
DEB = "uia_0.2.0_amd64.deb"
MAC = "uia_universal.app.tar.gz"


def build(names, sigs, require=("windows", "linux"), version="0.2.0"):
    return updater.build_manifest(
        version=version, notes="notes", pub_date="2026-10-09T00:00:00Z", tag=TAG,
        repo=REPO, asset_names=names, signatures=sigs, require=set(require),
    )


class BuildManifestTests(unittest.TestCase):
    def test_windows_and_linux(self):
        m = build([MSI, MSI + ".sig", DEB, DEB + ".sig"],
                  {MSI + ".sig": "SIGW\n", DEB + ".sig": "SIGL"})
        self.assertEqual(m["version"], "0.2.0")
        self.assertEqual(m["notes"], "notes")
        self.assertEqual(m["pub_date"], "2026-10-09T00:00:00Z")
        self.assertEqual(sorted(m["platforms"]), ["linux-x86_64-deb", "windows-x86_64-msi"])
        self.assertEqual(m["platforms"]["windows-x86_64-msi"]["signature"], "SIGW")
        # Built from the tag, never from a draft's `untagged-…` download URL,
        # which changes the moment the release is published.
        self.assertEqual(
            m["platforms"]["linux-x86_64-deb"]["url"],
            f"https://github.com/{REPO}/releases/download/{TAG}/{DEB}",
        )

    def test_macos_universal_serves_both_architectures(self):
        names = [MSI, MSI + ".sig", DEB, DEB + ".sig", MAC, MAC + ".sig"]
        sigs = {n: "S" for n in names if n.endswith(".sig")}
        m = build(names, sigs)
        self.assertEqual(
            sorted(m["platforms"]),
            ["darwin-aarch64", "darwin-x86_64", "linux-x86_64-deb", "windows-x86_64-msi"],
        )
        self.assertEqual(m["platforms"]["darwin-aarch64"], m["platforms"]["darwin-x86_64"])

    def test_missing_required_installer_fails(self):
        with self.assertRaisesRegex(updater.ManifestError, "no linux installer"):
            build([MSI, MSI + ".sig"], {MSI + ".sig": "S"})

    def test_missing_signature_fails(self):
        with self.assertRaisesRegex(updater.ManifestError, "has no .*\\.sig"):
            build([MSI, MSI + ".sig", DEB], {MSI + ".sig": "S"})

    def test_empty_signature_fails_like_a_missing_one(self):
        for empty in ("", "  \n"):
            with self.subTest(sig=repr(empty)):
                with self.assertRaisesRegex(updater.ManifestError, "has no .*\\.sig"):
                    build([MSI, MSI + ".sig", DEB, DEB + ".sig"],
                          {MSI + ".sig": "S", DEB + ".sig": empty})

    def test_two_installers_for_one_platform_fails(self):
        other = "uia_0.2.0_x64_fr-FR.msi"
        names = [MSI, MSI + ".sig", other, other + ".sig", DEB, DEB + ".sig"]
        with self.assertRaisesRegex(updater.ManifestError, "more than one windows"):
            build(names, {n: "S" for n in names if n.endswith(".sig")})

    def test_version_must_be_plain_semver(self):
        names = [MSI, MSI + ".sig", DEB, DEB + ".sig"]
        sigs = {n: "S" for n in names if n.endswith(".sig")}
        for bad in ("v0.2.0", "0.2.0-rc.1", "uai-app-0.2.0"):
            with self.assertRaises(ValueError):
                build(names, sigs, version=bad)


class UpdaterManifestCommandTests(unittest.TestCase):
    def test_writes_latest_json_from_the_release(self):
        release = {
            "body": "## Changes",
            "assets": [
                {"id": 1, "name": MSI}, {"id": 2, "name": MSI + ".sig"},
                {"id": 3, "name": DEB}, {"id": 4, "name": DEB + ".sig"},
            ],
        }

        def fake_gh_api(path, *extra):
            if path == f"repos/{REPO}/releases/42":
                return json.dumps(release)
            return {f"repos/{REPO}/releases/assets/2": "SIGW",
                    f"repos/{REPO}/releases/assets/4": "SIGL"}[path]

        out = Path(tempfile.mkdtemp()) / "latest.json"
        with mock.patch.object(cli, "_gh_api", side_effect=fake_gh_api), \
                contextlib.redirect_stdout(io.StringIO()):
            code = cli.main(["updater-manifest", "--repo", REPO, "--release-id", "42",
                             "--tag", TAG, "--version", "0.2.0",
                             "--require", "windows", "--require", "linux", "--out", str(out)])
        self.assertEqual(code, 0)
        written = json.loads(out.read_text())
        self.assertEqual(written["notes"], "## Changes")
        self.assertEqual(written["platforms"]["linux-x86_64-deb"]["signature"], "SIGL")

    def test_a_missing_signature_exits_nonzero(self):
        release = {"body": "", "assets": [{"id": 1, "name": MSI}, {"id": 3, "name": DEB}]}
        out = Path(tempfile.mkdtemp()) / "latest.json"
        err = io.StringIO()
        with mock.patch.object(cli, "_gh_api", return_value=json.dumps(release)), \
                contextlib.redirect_stderr(err):
            code = cli.main(["updater-manifest", "--repo", REPO, "--release-id", "42",
                             "--tag", TAG, "--version", "0.2.0",
                             "--require", "windows", "--out", str(out)])
        self.assertEqual(code, 2)
        self.assertIn("has no", err.getvalue())
        self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
