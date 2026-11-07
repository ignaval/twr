import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from source_probe import GitError, Repository


class SourceTransferTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="twr-source-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        isolated = patch.dict(os.environ, {
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.root / "xdg"),
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
        })
        isolated.start()
        self.addCleanup(isolated.stop)
        self.hub = self.repository("hub")
        (self.hub.path / "source").write_text("base\n")
        self.hub.git("add", ".")
        self.hub.git("commit", "-qm", "base")
        self.base = self.hub.git("rev-parse", "HEAD").decode()

    def repository(self, name):
        path = self.root / name
        path.mkdir()
        repo = Repository(path)
        repo.git("init", "-q")
        return repo

    @staticmethod
    def only_ref(bundle, repo):
        return repo.git("bundle", "list-heads", str(bundle)).decode().split()[1]

    def test_snapshot_effective_source_preserves_index_and_user_state(self):
        (self.hub.path / "source").write_text("staged\n")
        self.hub.git("add", "source")
        (self.hub.path / "source").write_text("unstaged\n")
        (self.hub.path / "new-source").write_text("untracked\n")
        (self.hub.path / ".gitignore").write_text("private\n")
        (self.hub.path / "private").write_text("not shipped\n")
        index = self.hub.path / ".git/index"
        before = index.read_bytes()
        snapshot = self.hub.snapshot()
        self.assertEqual(index.read_bytes(), before)
        self.assertEqual(self.hub.git("rev-parse", "HEAD").decode(), self.base)
        self.assertEqual(self.hub.git("show", snapshot + ":source"), b"unstaged")
        self.assertEqual(self.hub.git("show", snapshot + ":new-source"), b"untracked")
        with self.assertRaises(GitError):
            self.hub.git("show", snapshot + ":private")
        (self.hub.path / "source").write_text("after capture\n")
        self.assertEqual(self.hub.git("show", snapshot + ":source"), b"unstaged")
        self.assertNotEqual(snapshot, self.hub.snapshot())
        self.assertEqual(self.hub.git("stash", "list"), b"")

    def test_snapshot_does_not_run_repository_filters(self):
        marker = self.root / "filter-ran"
        self.hub.git("config", "filter.hostile.clean", f"touch {marker}; cat")
        self.hub.git("config", "filter.hostile.required", "true")
        (self.hub.path / ".gitattributes").write_text("source filter=hostile\n")
        (self.hub.path / "source").write_text("literal bytes\n")
        snapshot = self.hub.snapshot()
        self.assertFalse(marker.exists())
        self.assertEqual(self.hub.git("show", snapshot + ":source"), b"literal bytes")

    def test_snapshot_respects_global_ignore_file_without_enabling_global_filters(self):
        ignored = self.root / "global-ignore"
        ignored.write_text(".env\n")
        config = self.root / "gitconfig"
        marker = self.root / "global-filter-ran"
        self.hub.git("config", "--file", str(config), "core.excludesFile", str(ignored))
        self.hub.git("config", "--file", str(config), "filter.hostile.clean", f"touch {marker}; cat")
        (self.hub.path / ".env").write_text("private local value\n")
        (self.hub.path / ".gitattributes").write_text("source filter=hostile\n")
        with patch.dict(os.environ, {"GIT_CONFIG_GLOBAL": str(config)}):
            snapshot = self.hub.snapshot()
        with self.assertRaises(GitError):
            self.hub.git("show", snapshot + ":.env")
        self.assertFalse(marker.exists())

    def test_snapshot_refuses_hidden_local_edits_without_changing_index(self):
        for flag in ("skip-worktree", "assume-unchanged"):
            with self.subTest(flag=flag):
                self.hub.git("update-index", "--" + flag, "source")
                (self.hub.path / "source").write_text("hidden local private value\n")
                before = (self.hub.path / ".git/index").read_bytes()
                with self.assertRaisesRegex(GitError, "skip-worktree or assume-unchanged"):
                    self.hub.snapshot()
                self.assertEqual((self.hub.path / ".git/index").read_bytes(), before)
                self.assertEqual(self.hub.git("for-each-ref", "refs/twr/builds/"), b"")
                self.hub.git("update-index", "--no-" + flag, "source")

    def test_snapshot_respects_default_xdg_ignore_file(self):
        ignored = self.root / "xdg/git/ignore"
        ignored.parent.mkdir(parents=True)
        ignored.write_text(".env\n")
        (self.hub.path / ".env").write_text("private local value\n")
        snapshot = self.hub.snapshot()
        with self.assertRaises(GitError):
            self.hub.git("show", snapshot + ":.env")

    def test_no_origin_round_trip_does_not_change_hub_checkout(self):
        runner = self.repository("runner")
        inbound = self.root / "input.bundle"
        self.hub.bundle(self.base, inbound, ["f" * 40], 1 << 20)
        ref = self.only_ref(inbound, self.hub)
        runner.receive(inbound, ref, self.base, "refs/twr/sync/input")
        runner.git("checkout", "-qb", "twr/result", self.base)
        (runner.path / "result").write_text("finished\n")
        runner.git("add", "result")
        runner.git("commit", "-qm", "result")
        result = runner.git("rev-parse", "HEAD").decode()
        outbound = self.root / "result.bundle"
        runner.bundle(result, outbound, [self.base], 1 << 20)
        self.hub.receive(outbound, self.only_ref(outbound, runner), result,
                         "refs/twr/results/runner/thread")
        self.assertEqual(self.hub.git("rev-parse", "HEAD").decode(), self.base)
        self.assertEqual(self.hub.git("show", "refs/twr/results/runner/thread:result"), b"finished")
        self.assertFalse((self.hub.path / "result").exists())
        self.assertEqual(self.hub.git("remote"), b"")
        self.assertEqual(runner.git("remote"), b"")

    def test_snapshot_respects_ignore_path_from_git_environment_config(self):
        ignored = self.root / "env-ignore"
        ignored.write_text(".env\n")
        (self.hub.path / ".env").write_text("private local value\n")
        with patch.dict(os.environ, {"GIT_CONFIG_COUNT": "1",
                        "GIT_CONFIG_KEY_0": "core.excludesFile",
                        "GIT_CONFIG_VALUE_0": str(ignored)}):
            snapshot = self.hub.snapshot()
        with self.assertRaises(GitError):
            self.hub.git("show", snapshot + ":.env")

    def test_incremental_missing_prerequisites_requires_full_fallback(self):
        runner = self.repository("runner")
        (self.hub.path / "source").write_text("new commit\n")
        self.hub.git("add", ".")
        self.hub.git("commit", "-qm", "next")
        tip = self.hub.git("rev-parse", "HEAD").decode()
        incremental = self.root / "incremental.bundle"
        self.hub.bundle(tip, incremental, [self.base], 1 << 20)
        with self.assertRaises(GitError):
            runner.receive(incremental, self.only_ref(incremental, self.hub), tip,
                           "refs/twr/sync/input")
        complete = self.root / "complete.bundle"
        self.hub.bundle(tip, complete, [], 1 << 20)
        runner.receive(complete, self.only_ref(complete, self.hub), tip, "refs/twr/sync/input")
        self.assertEqual(runner.git("rev-parse", "refs/twr/sync/input").decode(), tip)

    def test_cap_and_wrong_tip_leave_no_published_reference(self):
        bundle = self.root / "too-large.bundle"
        with self.assertRaises(GitError):
            self.hub.bundle(self.base, bundle, [], 1)
        self.assertFalse(bundle.exists())
        self.assertEqual(self.hub.git("for-each-ref", "refs/twr/sync/"), b"")
        good = self.root / "good.bundle"
        self.hub.bundle(self.base, good, [], 1 << 20)
        runner = self.repository("runner")
        with self.assertRaises(GitError):
            runner.receive(good, self.only_ref(good, self.hub), "f" * 40,
                           "refs/twr/results/runner/thread")
        self.assertEqual(runner.git("for-each-ref"), b"")

    def test_inherited_git_environment_does_not_select_another_repository(self):
        original = os.environ.get("GIT_DIR")
        os.environ["GIT_DIR"] = str(self.root / "nonexistent")
        try:
            self.assertEqual(self.hub.git("rev-parse", "HEAD").decode(), self.base)
        finally:
            if original is None:
                del os.environ["GIT_DIR"]
            else:
                os.environ["GIT_DIR"] = original


if __name__ == "__main__":
    unittest.main()
