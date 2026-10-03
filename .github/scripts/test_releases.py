import copy
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

import releases


class HistoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.base = self.commit("baseline")
        config = self.root / ".github/release-base.json"
        config.parent.mkdir()
        config.write_text(json.dumps({"commit": self.base, "version": "0.1.0"}))

    def git(self, *args):
        return releases.git(self.root, *args)

    def commit(self, message):
        (self.root / "app.txt").write_text(message)
        self.git("add", "app.txt")
        self.git("commit", "-m", message)
        return self.git("rev-parse", "HEAD")

    def test_multi_commit_push_allocates_one_minor_per_commit_and_retry_is_stable(self):
        first, second = self.commit("Linux"), self.commit("automation")
        expected = [{"sha": first, "version": "0.2.0"},
                    {"sha": second, "version": "0.3.0"}]
        self.assertEqual(releases.history(self.root, second), expected)
        self.assertEqual(releases.history(self.root, second), expected)

    def test_merge_counts_once_and_does_not_release_side_branch_commits(self):
        self.git("checkout", "-b", "feature")
        self.commit("feature one")
        self.commit("feature two")
        self.git("checkout", "main")
        self.git("merge", "--no-ff", "feature", "-m", "merge feature")
        merged = self.git("rev-parse", "HEAD")
        self.assertEqual(releases.history(self.root, merged), [{"sha": merged, "version": "0.2.0"}])

    def test_history_rewrite_and_malformed_sha_fail_closed(self):
        self.git("checkout", "--orphan", "rewritten")
        head = self.commit("rewritten baseline")
        with self.assertRaisesRegex(ValueError, "baseline"):
            releases.history(self.root, head)
        with self.assertRaisesRegex(ValueError, "SHA"):
            releases.history(self.root, "HEAD; echo injected")

    def test_pending_recovers_missed_commits_but_skips_published_release(self):
        first, second = self.commit("first"), self.commit("second")
        remote = FakeGitHub()
        remote.tags["v0.2.0"] = first
        remote.releases["v0.2.0"] = {
            "id": 1, "draft": False, "prerelease": False,
            "assets": [{"name": name, "state": "uploaded", "size": 1}
                       for name in [releases.archive_name("0.2.0"), "SHA256SUMS"]]}
        self.assertEqual(releases.pending(self.root, second, remote.request),
                         [{"sha": second, "version": "0.3.0"}])


class FakeGitHub:
    def __init__(self):
        self.tags = {}
        self.releases = {}
        self.calls = []

    def request(self, path, method="GET", data=None, missing_ok=False):
        self.calls.append((path, method, data))
        if path.startswith("git/ref/tags/"):
            sha = self.tags.get(path.removeprefix("git/ref/tags/"))
            return {"object": {"type": "commit", "sha": sha}} if sha else None
        if path == "git/refs" and method == "POST":
            self.tags[data["ref"].removeprefix("refs/tags/")] = data["sha"]
            return {}
        if path.startswith("releases/tags/"):
            return copy.deepcopy(self.releases.get(path.removeprefix("releases/tags/")))
        if path == "releases" and method == "POST":
            release = {**data, "id": len(self.releases) + 1, "assets": []}
            self.releases[data["tag_name"]] = release
            return copy.deepcopy(release)
        if path.startswith("releases/"):
            release = next(r for r in self.releases.values() if str(r["id"]) == path.split("/")[1])
            if method == "PATCH":
                release.update(data)
            return copy.deepcopy(release)
        raise AssertionError(f"Unexpected API call: {method} {path}")

    def upload(self, paths):
        self.releases["v0.2.0"]["assets"] = [
            {"name": path.name, "state": "uploaded", "size": path.stat().st_size,
             "digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()} for path in paths]


class PolicyTests(unittest.TestCase):
    def permission(self, path):
        return {"permission": "admin" if "/owner/" in path else "write"}

    def test_owner_can_trigger_and_retry(self):
        releases.authorize(releases.REPOSITORY, "refs/heads/main", ["owner", "owner"], self.permission)

    def test_write_collaborator_cannot_trigger_or_rerun_owner_workflow(self):
        for actors in [["writer", "writer"], ["owner", "writer"]]:
            with self.assertRaises(PermissionError):
                releases.authorize(releases.REPOSITORY, "refs/heads/main", actors, self.permission)

    def test_forks_branches_invalid_actors_and_api_failures_cannot_publish(self):
        for repo, ref, actors in [("fork/repo", "refs/heads/main", ["owner"]),
                                  (releases.REPOSITORY, "refs/heads/feature", ["owner"]),
                                  (releases.REPOSITORY, "refs/heads/main", ["../../owner"]),
                                  (releases.REPOSITORY, "refs/heads/main", [None])]:
            with self.assertRaises(ValueError):
                releases.authorize(repo, ref, actors, self.permission)
        def unavailable(path):
            raise RuntimeError("API unavailable")
        with self.assertRaises(RuntimeError):
            releases.authorize(releases.REPOSITORY, "refs/heads/main", ["owner"], unavailable)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        binary = self.root / "binary"
        binary.write_bytes(b"synthetic executable fixture")
        (self.root / "README.md").write_text("Synthetic release fixture")
        self.output = self.root / "dist"
        releases.package(binary, self.root, self.output, "0.2.0", 1)
        self.release = {"sha": "a" * 40, "version": "0.2.0"}
        self.remote = FakeGitHub()

    def publish(self, upload=None):
        releases.publish(self.release, self.output, self.remote.request,
                         upload or self.remote.upload)

    def test_packaging_contains_only_executable_and_readme(self):
        with tarfile.open(self.output / releases.archive_name("0.2.0")) as archive:
            self.assertEqual(archive.getnames(), ["storage_analytics", "README.md"])
            self.assertEqual(archive.getmember("storage_analytics").mode, 0o755)

    def test_publish_then_retry_does_not_create_another_version(self):
        self.publish()
        calls = len(self.remote.calls)
        self.publish()
        self.assertEqual(self.remote.tags, {"v0.2.0": "a" * 40})
        self.assertFalse(self.remote.releases["v0.2.0"]["draft"])
        self.assertTrue(all(method == "GET" for _, method, _ in self.remote.calls[calls:]))

    def test_upload_failure_leaves_a_draft_and_retry_finishes_it(self):
        def broken_upload(paths):
            raise RuntimeError("Upload interrupted")
        with self.assertRaises(RuntimeError):
            self.publish(broken_upload)
        self.assertTrue(self.remote.releases["v0.2.0"]["draft"])
        self.publish()
        self.assertEqual(len(self.remote.releases), 1)
        self.assertFalse(self.remote.releases["v0.2.0"]["draft"])

    def test_tag_collision_is_rejected_without_mutation(self):
        self.remote.tags["v0.2.0"] = "b" * 40
        with self.assertRaisesRegex(ValueError, "different commit"):
            self.publish()
        self.assertTrue(all(method == "GET" for _, method, _ in self.remote.calls))

    def test_bad_uploaded_digest_does_not_publish(self):
        def corrupted(paths):
            self.remote.upload(paths)
            self.remote.releases["v0.2.0"]["assets"][0]["digest"] = "sha256:incorrect"
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.publish(corrupted)
        self.assertTrue(self.remote.releases["v0.2.0"]["draft"])

    def test_published_incomplete_release_is_not_overwritten(self):
        self.remote.releases["v0.2.0"] = {"id": 1, "draft": False, "assets": []}
        with self.assertRaisesRegex(ValueError, "incomplete"):
            self.publish()
        self.assertTrue(all(method == "GET" for _, method, _ in self.remote.calls))

    def test_bad_local_checksum_is_rejected_before_creating_tag_or_draft(self):
        (self.output / "SHA256SUMS").write_text("corrupted checksum")
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.publish()
        self.assertTrue(all(method == "GET" for _, method, _ in self.remote.calls))

    def test_tag_deleted_during_upload_is_not_recreated_at_publication(self):
        def removed_tag(paths):
            self.remote.upload(paths)
            del self.remote.tags["v0.2.0"]
        with self.assertRaisesRegex(ValueError, "disappeared"):
            self.publish(removed_tag)
        self.assertTrue(self.remote.releases["v0.2.0"]["draft"])


if __name__ == "__main__":
    unittest.main()
