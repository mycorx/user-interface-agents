"""scripts/ci_changes.py: which pull requests make CI run its jobs."""
import unittest

import ci_changes
from helpers import load_registry_from, make_repo


class FirstCodeFileTests(unittest.TestCase):
    def setUp(self):
        self.components = load_registry_from(make_repo())

    def hit(self, *files):
        return ci_changes.first_code_file(files, self.components)

    def test_docs_and_markdown_do_not_trigger(self):
        self.assertIsNone(self.hit("README.md", "docs/RELEASE.md", "CONTRIBUTING.md"))
        self.assertIsNone(self.hit(".github/pull_request_template.md", ".github/CODEOWNERS"))

    def test_registry_code_paths_trigger(self):
        self.assertEqual(self.hit("README.md", "src/App.svelte"), "src/App.svelte")
        self.assertEqual(self.hit("package.json"), "package.json")

    def test_what_ci_itself_runs_triggers(self):
        for f in (
            ".github/workflows/ci.yaml",
            ".github/release-components.json",
            "scripts/gen-notice.sh",
            "NOTICE.txt",
            "LICENSES/MIT.txt",
            "uia.example.toml",
        ):
            self.assertEqual(self.hit(f), f)

    def test_empty_diff_does_not_trigger(self):
        self.assertIsNone(self.hit())


if __name__ == "__main__":
    unittest.main()
