import json
import unittest
from pathlib import Path

from helpers import REGISTRY, load_registry_from, make_repo
from release import registry as reg
from release import sync


class RegistryTests(unittest.TestCase):
    def setUp(self):
        self.comp = load_registry_from(make_repo())["uia-app"]

    def test_tag_round_trip(self):
        self.assertEqual(self.comp.tag_for("1.2.3"), "uia-app-1.2.3")
        self.assertEqual(self.comp.version_from_tag("uia-app-1.2.3"), "1.2.3")

    def test_foreign_and_malformed_tags_are_not_ours(self):
        for tag in ("0.1.1", "uia-mobile-1.2.3", "uia-app-1.2", "uia-app-1.2.3-rc.1", "uia-app-"):
            self.assertIsNone(self.comp.version_from_tag(tag), tag)

    def test_tag_must_contain_version_once(self):
        root = make_repo()
        bad = json.loads(json.dumps(REGISTRY))
        bad["uia-app"]["tag"] = "uia-app"
        (root / ".github/release-components.json").write_text(json.dumps(bad))
        with self.assertRaises(reg.RegistryError):
            load_registry_from(root)

    def test_duplicate_labels_are_rejected(self):
        root = make_repo()
        bad = json.loads(json.dumps(REGISTRY))
        bad["other"] = json.loads(json.dumps(REGISTRY["uia-app"]))
        (root / ".github/release-components.json").write_text(json.dumps(bad))
        with self.assertRaises(reg.RegistryError):
            load_registry_from(root)

    def test_versions_must_be_plain(self):
        root = make_repo()
        (root / "version.json").write_text('{"uia-app": "1.2"}')
        with self.assertRaises(reg.RegistryError):
            reg.read_versions(root)


class SyncTests(unittest.TestCase):
    def setUp(self):
        self.root = make_repo("0.1.1")
        self.comp = load_registry_from(self.root)["uia-app"]

    def versions(self):
        return {p: v for p, v in sync.read_synced(self.root, self.comp)}

    def test_reads_all_three_files(self):
        self.assertEqual(
            self.versions(),
            {
                "crates/uia-app/tauri.conf.json": "0.1.1",
                "crates/uia-app/Cargo.toml": "0.1.1",
                "Cargo.lock": "0.1.1",
            },
        )

    def test_apply_changes_all_three_and_only_the_right_lines(self):
        before = {p: (self.root / p).read_text() for p in self.versions()}
        sync.apply(self.root, self.comp, "0.2.0")
        self.assertEqual(set(self.versions().values()), {"0.2.0"})
        for path, old in before.items():
            changed = [
                (a, b)
                for a, b in zip(old.splitlines(), (self.root / path).read_text().splitlines())
                if a != b
            ]
            self.assertEqual(len(changed), 1, f"{path}: exactly one line should change: {changed}")

    def test_decoy_versions_are_untouched(self):
        sync.apply(self.root, self.comp, "0.2.0")
        tauri = (self.root / "crates/uia-app/tauri.conf.json").read_text()
        self.assertIn('"version": "9.9.9"', tauri)
        self.assertIn('"version": "decoy"', tauri)
        cargo = (self.root / "crates/uia-app/Cargo.toml").read_text()
        self.assertIn('serde = { version = "1" }', cargo)
        self.assertIn('version = "2.11.5"', cargo)
        self.assertIn("# kept in step by scripts/release", cargo)
        self.assertIn('name = "serde"\nversion = "1.0.1"', (self.root / "Cargo.lock").read_text())

    def test_apply_is_idempotent(self):
        sync.apply(self.root, self.comp, "0.3.0")
        snapshot = {p: (self.root / p).read_text() for p in self.versions()}
        sync.apply(self.root, self.comp, "0.3.0")
        self.assertEqual(snapshot, {p: (self.root / p).read_text() for p in self.versions()})

    def test_check_names_each_file_that_disagrees(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('name = "uia-app"\nversion = "0.1.1"', 'name = "uia-app"\nversion = "0.1.0"'))
        errors = sync.check(self.root, self.comp, "0.1.1")
        self.assertEqual(len(errors), 1)
        self.assertIn("Cargo.lock", errors[0])
        self.assertIn("0.1.0", errors[0])

    def test_check_passes_when_in_step(self):
        self.assertEqual(sync.check(self.root, self.comp, "0.1.1"), [])

    def test_a_workspace_inherited_version_is_an_error_not_a_guess(self):
        path = self.root / "crates/uia-app/Cargo.toml"
        path.write_text(path.read_text().replace('version = "0.1.1"', "version.workspace = true"))
        self.assertTrue(sync.check(self.root, self.comp, "0.1.1"))

    def test_missing_lock_entry_is_an_error(self):
        path = self.root / "Cargo.lock"
        path.write_text(path.read_text().replace('name = "uia-app"', 'name = "other"'))
        with self.assertRaises(sync.SyncError):
            sync.apply(self.root, self.comp, "0.2.0")


class RealRepoTests(unittest.TestCase):
    """The checked-in registry, version.json and synced files agree."""

    ROOT = Path(__file__).resolve().parents[3]

    def test_every_component_is_in_step(self):
        registry = load_registry_from(self.ROOT)
        versions = reg.read_versions(self.ROOT)
        for comp in registry.values():
            self.assertIn(comp.name, versions)
            self.assertEqual(sync.check(self.ROOT, comp, versions[comp.name]), [], comp.name)


if __name__ == "__main__":
    unittest.main()
