import contextlib
import io
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from helpers import GOOD_BODY, make_repo
from release import cli


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = cli.main(list(argv))
    return code, out.getvalue(), err.getvalue()


def write(path, text):
    Path(path).write_text(text)
    return str(path)


class PrCommandTests(unittest.TestCase):
    def setUp(self):
        self.base = make_repo("0.1.1")
        self.head = make_repo("0.1.1")
        tmp = Path(tempfile.mkdtemp())
        self.files = write(tmp / "files", "crates/uia-app/src/lib.rs\n")
        self.body = write(tmp / "body", GOOD_BODY)

    def args(self, cmd, label="semver:uai-app:patch", extra=()):
        return [cmd, "--root", str(self.head), "--base-root", str(self.base),
                "--label", label, "--changed-files", self.files, "--author", "alice", *extra]

    def test_unbumped_pr_fails_check(self):
        code, out, _ = run(*self.args("check-pr", extra=["--body-file", self.body]))
        self.assertEqual(code, 1)
        self.assertIn("expected 0.1.2", out)

    def test_bump_then_check_passes(self):
        code, out, _ = run(*self.args("bump"))
        self.assertEqual(code, 0)
        self.assertIn("0.1.1 -> 0.1.2", out)
        self.assertEqual(json.loads((self.head / "version.json").read_text()), {"uai-app": "0.1.2"})
        self.assertIn('"version": "0.1.2"', (self.head / "crates/uia-app/tauri.conf.json").read_text())
        self.assertIn('version = "0.1.2"', (self.head / "crates/uia-app/Cargo.toml").read_text())
        code, out, _ = run(*self.args("check-pr", extra=["--body-file", self.body]))
        self.assertEqual((code, out.strip()), (0, "release-check passed"))

    def test_bump_twice_changes_nothing_the_second_time(self):
        run(*self.args("bump"))
        code, out, _ = run(*self.args("bump"))
        self.assertEqual(code, 0)
        self.assertIn("already correct", out)

    def test_relabelling_to_a_smaller_bump_resets_the_version(self):
        run(*self.args("bump", label="semver:uai-app:minor"))
        self.assertEqual(json.loads((self.head / "version.json").read_text()), {"uai-app": "0.2.0"})
        run(*self.args("bump", label="semver:uai-app:patch"))
        self.assertEqual(json.loads((self.head / "version.json").read_text()), {"uai-app": "0.1.2"})

    def test_a_renovate_rebase_that_drops_the_bump_is_bumped_again(self):
        run(*self.args("bump"))
        reset = make_repo("0.1.1", self.head)  # what a rebase leaves: base's files
        self.assertEqual(reset, self.head)
        code, out, _ = run(*self.args("bump"))
        self.assertEqual(code, 0)
        self.assertIn("0.1.1 -> 0.1.2", out)
        self.assertEqual(run(*self.args("check-pr", extra=["--body-file", self.body]))[0], 0)

    def test_bump_with_bad_labels_changes_nothing_and_does_not_fail(self):
        code, out, _ = run(*self.args("bump", label="semver:uai-app:none"))
        self.assertEqual(code, 0)
        self.assertIn("not bumping", out)
        self.assertEqual(json.loads((self.head / "version.json").read_text()), {"uai-app": "0.1.1"})

    def test_manifest_drift_fails_check(self):
        run(*self.args("bump"))
        lock = self.head / "Cargo.lock"
        lock.write_text(lock.read_text().replace('name = "uia-app"\nversion = "0.1.2"', 'name = "uia-app"\nversion = "0.1.1"'))
        code, out, _ = run(*self.args("check-pr", extra=["--body-file", self.body]))
        self.assertEqual(code, 1)
        self.assertIn("Cargo.lock", out)

    def test_the_registry_is_read_from_base_not_head(self):
        (self.head / ".github/release-components.json").write_text("{}")
        code, out, _ = run(*self.args("check-pr", extra=["--body-file", self.body]))
        self.assertEqual(code, 1)  # judged by the base's registry, so still unbumped
        self.assertIn("expected 0.1.2", out)


class EventInputTests(unittest.TestCase):
    def test_labels_author_and_body_come_from_the_event_payload(self):
        base, head = make_repo("0.1.1"), make_repo("0.1.1")
        tmp = Path(tempfile.mkdtemp())
        event = write(tmp / "event.json", json.dumps({"pull_request": {
            "number": 7, "user": {"login": "alice"}, "body": GOOD_BODY,
            "labels": [{"name": "bug"}, {"name": "semver:uai-app:patch"}],
        }}))
        files = write(tmp / "files", "crates/uia-app/src/lib.rs\n")
        argv = ["--root", str(head), "--base-root", str(base), "--event", event, "--changed-files", files]
        self.assertEqual(run("check-pr", *argv)[0], 1)  # not bumped yet
        self.assertEqual(run("bump", *argv)[0], 0)
        self.assertEqual(run("check-pr", *argv)[0], 0)

    def test_no_files_and_no_event_is_an_error(self):
        base = make_repo()
        code, _, err = run("check-pr", "--root", str(base), "--base-root", str(base), "--label", "x")
        self.assertEqual(code, 2)
        self.assertIn("--changed-files", err)


class SyncCommandTests(unittest.TestCase):
    def test_sync_check_and_expect_version(self):
        root = make_repo("0.1.1")
        self.assertEqual(run("sync", "--root", str(root), "--check")[0], 0)
        self.assertEqual(run("sync", "--root", str(root), "--check", "--expect-version", "0.1.2")[0], 1)
        (root / "version.json").write_text('{"uai-app": "0.3.0"}')
        self.assertEqual(run("sync", "--root", str(root), "--check")[0], 1)
        self.assertEqual(run("sync", "--root", str(root))[0], 0)
        self.assertEqual(run("sync", "--root", str(root), "--check")[0], 0)

    def test_current(self):
        root = make_repo("0.4.5")
        self.assertEqual(run("current", "--root", str(root), "--component", "uai-app")[1].strip(), "0.4.5")

    def test_unknown_component_is_a_clear_error(self):
        code, _, err = run("current", "--root", str(make_repo()), "--component", "nope")
        self.assertEqual(code, 2)
        self.assertIn("unknown component", err)


def git(root, *args):
    subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)


class GitCommandTests(unittest.TestCase):
    def setUp(self):
        self.root = make_repo("0.1.2")
        git(self.root, "init", "-q", "-b", "main")
        git(self.root, "config", "user.email", "t@example.com")
        git(self.root, "config", "user.name", "t")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "one")
        git(self.root, "tag", "uai-app-0.1.1")
        (self.root / "x").write_text("x")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "two")
        git(self.root, "tag", "uai-app-0.1.2")
        git(self.root, "update-ref", "refs/remotes/origin/main", "HEAD")
        self.published = write(Path(tempfile.mkdtemp()) / "pub", "uai-app-0.1.1\n")

    def test_plan_picks_the_newest_tag_after_the_baseline(self):
        code, out, _ = run("plan", "--root", str(self.root), "--component", "uai-app",
                           "--published-tags-file", self.published)
        self.assertEqual(code, 0)
        plan = json.loads(out)
        self.assertEqual((plan["baseline"], plan["target"]), ("uai-app-0.1.1", "uai-app-0.1.2"))

    def test_plan_with_nothing_new_fails(self):
        pub = write(Path(tempfile.mkdtemp()) / "pub", "uai-app-0.1.2\n")
        code, _, err = run("plan", "--root", str(self.root), "--component", "uai-app",
                           "--published-tags-file", pub)
        self.assertEqual(code, 2)
        self.assertIn("nothing to release", err)

    def test_tag_dry_run_reports_the_missing_tag(self):
        (self.root / "version.json").write_text('{"uai-app": "0.1.3"}')
        code, out, _ = run("tag", "--root", str(self.root), "--sha", "abc", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("would tag abc as uai-app-0.1.3", out)

    def test_tag_with_every_version_tagged_does_nothing(self):
        code, out, _ = run("tag", "--root", str(self.root), "--sha", "abc", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("already has a tag", out)


if __name__ == "__main__":
    unittest.main()
