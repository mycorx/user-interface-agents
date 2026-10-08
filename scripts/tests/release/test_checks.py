import unittest

from helpers import GOOD_BODY, load_registry_from, make_repo, two_component_registry
from release import checks, labels, prbody

CODE = ["crates/uia-app/src/lib.rs"]
DOCS = ["docs/RELEASE.md", ".github/workflows/ci.yaml"]


class LabelTests(unittest.TestCase):
    def setUp(self):
        self.registry = load_registry_from(make_repo())

    def run_labels(self, lbls, files, renovate=False):
        return labels.check_labels(self.registry, lbls, files, renovate)

    def test_code_with_a_bump_label_is_fine(self):
        chosen, errors = self.run_labels(["semver:uai-app:patch"], CODE)
        self.assertEqual((chosen, errors), ({"uai-app": "patch"}, []))

    def test_no_semver_label_is_an_error(self):
        _, errors = self.run_labels(["bug"], CODE)
        self.assertEqual(len(errors), 1)
        self.assertIn("no semver label", errors[0])

    def test_two_labels_for_one_component_is_an_error(self):
        _, errors = self.run_labels(["semver:uai-app:patch", "semver:uai-app:minor"], CODE)
        self.assertTrue(any("more than one" in e for e in errors))

    def test_unknown_component_malformed_and_unknown_bump(self):
        for bad in ("semver:uai-mobile:patch", "semver:patch", "semver:uai-app:huge"):
            _, errors = self.run_labels([bad], CODE)
            self.assertTrue(errors, bad)

    def test_none_is_rejected_when_code_changed(self):
        _, errors = self.run_labels(["semver:uai-app:none"], CODE)
        self.assertTrue(any("not allowed" in e for e in errors))

    def test_none_is_accepted_when_only_docs_changed(self):
        self.assertEqual(self.run_labels(["semver:uai-app:none"], DOCS)[1], [])

    def test_a_bump_is_rejected_when_nothing_shippable_changed(self):
        _, errors = self.run_labels(["semver:uai-app:patch"], DOCS)
        self.assertTrue(any("release nothing" in e for e in errors))

    def test_renovate_may_label_none_over_code(self):
        self.assertEqual(self.run_labels(["semver:uai-app:none"], CODE, renovate=True)[1], [])

    def test_renovate_still_needs_exactly_a_label(self):
        self.assertTrue(self.run_labels([], CODE, renovate=True)[1])

    def test_expected_versions(self):
        got = labels.expected_versions(self.registry, {"uai-app": "0.1.1"}, {"uai-app": "minor"})
        self.assertEqual(got, {"uai-app": "0.2.0"})
        got = labels.expected_versions(self.registry, {"uai-app": "0.1.1"}, {})
        self.assertEqual(got, {"uai-app": "0.1.1"})


class TwoComponentTests(unittest.TestCase):
    """A PR can touch more than one component; each needs its own label."""

    def setUp(self):
        self.registry = two_component_registry()

    def test_each_touched_component_needs_a_label(self):
        files = CODE + ["mobile/app.kt"]
        _, errors = labels.check_labels(self.registry, ["semver:uai-app:patch"], files, False)
        self.assertEqual(len(errors), 1)
        self.assertIn("uai-mobile", errors[0])

    def test_both_labelled_is_fine_and_bumps_each(self):
        files = CODE + ["mobile/app.kt"]
        chosen, errors = labels.check_labels(
            self.registry, ["semver:uai-app:patch", "semver:uai-mobile:minor"], files, False)
        self.assertEqual((chosen, errors), ({"uai-app": "patch", "uai-mobile": "minor"}, []))
        got = labels.expected_versions(
            self.registry, {"uai-app": "0.1.1", "uai-mobile": "1.0.0"}, chosen)
        self.assertEqual(got, {"uai-app": "0.1.2", "uai-mobile": "1.1.0"})

    def test_a_label_for_the_component_that_did_not_change_is_rejected(self):
        _, errors = labels.check_labels(
            self.registry, ["semver:uai-app:patch", "semver:uai-mobile:patch"], CODE, False)
        self.assertEqual(len(errors), 1)
        self.assertIn("uai-mobile", errors[0])

    def test_tags_are_per_component(self):
        self.assertEqual(self.registry["uai-mobile"].tag_for("1.0.0"), "uai-mobile-1.0.0")
        self.assertIsNone(self.registry["uai-app"].version_from_tag("uai-mobile-1.0.0"))


class BodyTests(unittest.TestCase):
    def test_good_body_passes(self):
        self.assertEqual(prbody.check_body(GOOD_BODY, True), [])

    def test_missing_section_named(self):
        errors = prbody.check_body("## Summary\n\nx\n", False)
        self.assertEqual(len(errors), 2)

    def test_untouched_template_placeholders_count_as_empty(self):
        body = "## Summary\n<!-- what and why -->\n## Release note\n<!-- one sentence -->\n## Testing\n<!-- how -->\n"
        self.assertEqual(len(prbody.check_body(body, False)), 3)

    def test_na_release_note_allowed_only_without_a_bump(self):
        body = GOOD_BODY.replace("Adds a thing users asked for.", "N/A")
        self.assertEqual(prbody.check_body(body, False), [])
        self.assertTrue(prbody.check_body(body, True))

    def test_heading_level_and_case_do_not_matter(self):
        body = "# summary\nx\n### RELEASE NOTE\ny\n## Testing\nz\n"
        self.assertEqual(prbody.check_body(body, True), [])

    def test_a_body_with_windows_line_endings_parses(self):
        # The GitHub web editor submits CRLF.
        crlf = GOOD_BODY.replace("\n", "\r\n")
        self.assertEqual(prbody.check_body(crlf, True), [])
        self.assertEqual(prbody.release_note(crlf), "Adds a thing users asked for.")

    def test_release_note_extraction(self):
        self.assertEqual(prbody.release_note(GOOD_BODY), "Adds a thing users asked for.")
        self.assertIsNone(prbody.release_note(GOOD_BODY.replace("Adds a thing users asked for.", "N/A")))
        self.assertIsNone(prbody.release_note(""))

    def test_multiline_note_is_collapsed(self):
        body = "## Release note\nline one\nline two\n"
        self.assertEqual(prbody.release_note(body), "line one line two")


class CheckPrTests(unittest.TestCase):
    def setUp(self):
        self.registry = load_registry_from(make_repo())

    def run_check(self, **over):
        args = dict(
            registry=self.registry, labels=["semver:uai-app:patch"], changed_files=CODE,
            author="alice", body=GOOD_BODY, base_versions={"uai-app": "0.1.1"},
            head_versions={"uai-app": "0.1.2"}, sync_errors=[],
        )
        args.update(over)
        return checks.check_pr(**args)

    def test_a_correct_pr_passes(self):
        self.assertEqual(self.run_check(), [])

    def test_unbumped_pr_names_the_version_it_needs(self):
        errors = self.run_check(head_versions={"uai-app": "0.1.1"})
        self.assertEqual(len(errors), 1)
        self.assertIn("expected 0.1.2", errors[0])

    def test_bump_hint_is_a_command_that_works_as_printed(self):
        errors = self.run_check(head_versions={"uai-app": "0.1.1"})
        for flag in ("--root", "--base-root", "--label", "--changed-files"):
            self.assertIn(flag, errors[0])
        self.assertNotIn("bot", errors[0])

    def test_a_stale_bump_is_caught_when_base_moved(self):
        errors = self.run_check(base_versions={"uai-app": "0.1.2"})
        self.assertIn("expected 0.1.3", errors[0])

    def test_none_pr_must_leave_the_version_alone(self):
        errors = self.run_check(
            labels=["semver:uai-app:none"], changed_files=DOCS,
            body=GOOD_BODY.replace("Adds a thing users asked for.", "N/A"),
        )
        self.assertTrue(any("expected 0.1.1" in e for e in errors))

    def test_sync_drift_is_reported(self):
        errors = self.run_check(sync_errors=["Cargo.lock says 0.1.0"])
        self.assertEqual(errors, ["Cargo.lock says 0.1.0"])

    def test_renovate_skips_the_body_check(self):
        errors = self.run_check(author="renovate[bot]", body="bump serde")
        self.assertEqual(errors, [])

    def test_a_human_with_an_empty_body_fails(self):
        self.assertTrue(self.run_check(body=""))


if __name__ == "__main__":
    unittest.main()
