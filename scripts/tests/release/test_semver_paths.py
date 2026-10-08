import unittest

from release import paths, semver


class SemverTests(unittest.TestCase):
    def test_bump_kinds(self):
        self.assertEqual(semver.bump("0.1.1", "patch"), "0.1.2")
        self.assertEqual(semver.bump("0.1.9", "minor"), "0.2.0")
        self.assertEqual(semver.bump("0.9.9", "major"), "1.0.0")
        self.assertEqual(semver.bump("1.2.3", "none"), "1.2.3")

    def test_double_digit_parts_are_not_compared_as_strings(self):
        self.assertGreater(semver.parse("0.10.0"), semver.parse("0.9.0"))

    def test_rejects_anything_but_plain_versions(self):
        for bad in ("v1.2.3", "1.2", "1.2.3-rc.1", "01.2.3", "1.2.3.4", ""):
            with self.assertRaises(ValueError, msg=bad):
                semver.parse(bad)

    def test_unknown_bump_kind(self):
        with self.assertRaises(ValueError):
            semver.bump("1.0.0", "huge")


class PathTests(unittest.TestCase):
    def test_double_star_crosses_directories(self):
        self.assertTrue(paths.matches("crates/**", "crates/uia-app/src/main.rs"))
        self.assertFalse(paths.matches("crates/**", "scripts/crates/x"))

    def test_single_star_stays_in_a_segment(self):
        self.assertTrue(paths.matches("src/*.ts", "src/a.ts"))
        self.assertFalse(paths.matches("src/*.ts", "src/lib/a.ts"))

    def test_leading_double_star_matches_root_and_nested(self):
        self.assertTrue(paths.matches("**/*.md", "README.md"))
        self.assertTrue(paths.matches("**/*.md", "docs/a/b.md"))
        self.assertFalse(paths.matches("**/*.md", "docs/a/b.mdx"))

    def test_literal_dots_are_not_wildcards(self):
        self.assertFalse(paths.matches("Cargo.toml", "CargoXtoml"))

    def test_touches_ignores_ignored_paths_and_non_code(self):
        code, ignore = ["crates/**", "Cargo.lock"], ["**/*.md"]
        self.assertTrue(paths.touches(code, ignore, ["docs/x.md", "crates/a/b.rs"]))
        self.assertFalse(paths.touches(code, ignore, ["crates/a/NOTES.md", "docs/x.md"]))
        self.assertFalse(paths.touches(code, ignore, []))


if __name__ == "__main__":
    unittest.main()
