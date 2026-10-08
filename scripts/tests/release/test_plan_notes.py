import unittest

from helpers import load_registry_from, make_repo
from release import notes, plan


class PlanTests(unittest.TestCase):
    def setUp(self):
        self.registry = load_registry_from(make_repo())
        self.comp = self.registry["uai-app"]
        self.tags = {
            "uai-app-0.1.1": "a1", "uai-app-0.1.2": "a2", "uai-app-0.1.10": "a10",
            "uai-app-0.1.3": "a3", "other-1.0.0": "zz", "0.1.1": "old",
        }
        self.yes = lambda *a: True

    def test_promotes_the_newest_tag_after_the_published_baseline(self):
        p = plan.make_plan(self.comp, self.tags, {"uai-app-0.1.1"}, self.yes, self.yes)
        self.assertEqual(p.baseline, "uai-app-0.1.1")
        self.assertEqual(p.target, "uai-app-0.1.10")
        self.assertEqual(p.candidates, ("uai-app-0.1.2", "uai-app-0.1.3", "uai-app-0.1.10"))
        self.assertEqual(p.target_sha, "a10")

    def test_a_published_release_of_another_component_is_not_our_baseline(self):
        p = plan.make_plan(self.comp, self.tags, {"other-1.0.0", "uai-app-0.1.2"}, self.yes, self.yes)
        self.assertEqual(p.baseline, "uai-app-0.1.2")
        self.assertEqual(p.candidates, ("uai-app-0.1.3", "uai-app-0.1.10"))

    def test_nothing_newer_is_an_error(self):
        with self.assertRaisesRegex(plan.PlanError, "nothing to release"):
            plan.make_plan(self.comp, self.tags, {"uai-app-0.1.10"}, self.yes, self.yes)

    def test_no_published_release_means_every_tag_counts(self):
        p = plan.make_plan(self.comp, self.tags, set(), self.yes, self.yes)
        self.assertEqual(p.baseline, None)
        self.assertEqual(len(p.candidates), 4)

    def test_a_published_release_whose_tag_was_deleted_still_sets_the_baseline(self):
        tags = {k: v for k, v in self.tags.items() if k != "uai-app-0.1.1"}
        p = plan.make_plan(self.comp, tags, {"uai-app-0.1.1"}, self.yes, self.yes)
        self.assertEqual(p.baseline, "uai-app-0.1.1")
        self.assertEqual(p.candidates, ("uai-app-0.1.2", "uai-app-0.1.3", "uai-app-0.1.10"))

    def test_target_must_descend_from_the_baseline(self):
        with self.assertRaisesRegex(plan.PlanError, "not a descendant"):
            plan.make_plan(self.comp, self.tags, {"uai-app-0.1.1"}, lambda a, b: False, self.yes)

    def test_target_must_be_on_main(self):
        with self.assertRaisesRegex(plan.PlanError, "not reachable from main"):
            plan.make_plan(self.comp, self.tags, {"uai-app-0.1.1"}, self.yes, lambda sha: False)


class TagTests(unittest.TestCase):
    def setUp(self):
        self.registry = load_registry_from(make_repo())

    def test_creates_the_missing_tag(self):
        got = plan.tags_to_create(self.registry, {"uai-app": "0.1.2"}, {"uai-app-0.1.1"})
        self.assertEqual(got, [("uai-app", "uai-app-0.1.2")])

    def test_existing_tag_means_nothing_to_do(self):
        self.assertEqual(plan.tags_to_create(self.registry, {"uai-app": "0.1.1"}, {"uai-app-0.1.1"}), [])

    def test_a_component_with_no_tags_at_all_refuses_to_guess(self):
        with self.assertRaisesRegex(plan.TagError, "no tags at all"):
            plan.tags_to_create(self.registry, {"uai-app": "0.1.1"}, {"0.1.1", "0.1.0"})
        with self.assertRaisesRegex(plan.TagError, "LAST PUBLISHED release"):
            plan.tags_to_create(self.registry, {"uai-app": "0.1.2"}, {"0.1.1"})

    def test_versions_must_not_go_backwards(self):
        with self.assertRaisesRegex(plan.TagError, "only go up"):
            plan.tags_to_create(self.registry, {"uai-app": "0.1.1"}, {"uai-app-0.1.5"})


class NotesTests(unittest.TestCase):
    def setUp(self):
        self.comp = load_registry_from(make_repo())["uai-app"]

    def pr(self, n, label, note, author="alice", title="t"):
        body = f"## Release note\n{note}\n" if note else ""
        return {"number": n, "title": title, "url": f"https://x/{n}", "author": author,
                "labels": [f"semver:uai-app:{label}"], "body": body}

    def test_groups_by_bump_with_breaking_first(self):
        out = notes.build_notes([
            self.pr(3, "patch", "Fixed a crash."), self.pr(1, "major", "Dropped the old config."),
            self.pr(2, "minor", "Added dark mode."),
        ], self.comp)
        self.assertLess(out.index("Breaking changes"), out.index("Features"))
        self.assertLess(out.index("Features"), out.index("Fixes"))
        self.assertIn("- Fixed a crash. ([#3](https://x/3))", out)

    def test_renovate_prs_go_under_dependencies_by_title(self):
        out = notes.build_notes([
            self.pr(5, "patch", None, author="renovate[bot]", title="Update serde to v2"),
            self.pr(6, "none", None, author="renovate[bot]", title="Update tokio"),
        ], self.comp)
        self.assertIn("## Dependencies", out)
        self.assertIn("Update serde to v2", out)
        self.assertIn("Update tokio", out)

    def test_none_labelled_human_prs_are_left_out(self):
        out = notes.build_notes([self.pr(7, "none", "N/A")], self.comp)
        self.assertEqual(out, "No user-facing changes.")

    def test_duplicates_are_listed_once(self):
        out = notes.build_notes([self.pr(1, "patch", "A."), self.pr(1, "patch", "A.")], self.comp)
        self.assertEqual(out.count("#1"), 1)


if __name__ == "__main__":
    unittest.main()
