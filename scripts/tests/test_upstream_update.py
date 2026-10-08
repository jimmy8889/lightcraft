"""Exercise merge preparation against disposable repositories, including conflicts."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "upstream-update.py"


class UpdateSafety(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="lightcraft-update-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.upstream = self.root / "upstream"
        self.env = os.environ.copy()
        # Test author identities are fixtures, not the user's profile or contributors list.
        self.env.update(GIT_AUTHOR_NAME="Update test", GIT_AUTHOR_EMAIL="test@example.invalid",
                        GIT_COMMITTER_NAME="Update test", GIT_COMMITTER_EMAIL="test@example.invalid")
        self.source.mkdir()
        self.git(self.source, "init", "-b", "main")
        (self.source / ".gitignore").write_text("plan/\n")
        (self.source / "feature.txt").write_text("upstream base\n")
        self.commit(self.source, "base")
        self.git(self.root, "clone", str(self.source), str(self.upstream))
        self.git(self.source, "remote", "add", "upstream", str(self.upstream))
        self.git(self.source, "switch", "-c", "custom/test")
        (self.source / "custom.txt").write_text("custom functionality\n")
        self.commit(self.source, "custom")
        self.before = self.git(self.source, "rev-parse", "HEAD").stdout.strip()

    def git(self, cwd, *args):
        result = subprocess.run(["git", *args], cwd=cwd, env=self.env, text=True,
                                capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def commit(self, cwd, message):
        self.git(cwd, "add", ".")
        self.git(cwd, "commit", "-m", message)

    def invoke(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), *args], cwd=self.source,
                              env=self.env, text=True, capture_output=True, check=False)

    def unchanged(self):
        self.assertEqual(self.git(self.source, "rev-parse", "HEAD").stdout.strip(), self.before)
        self.assertEqual(self.git(self.source, "symbolic-ref", "--short", "HEAD").stdout.strip(), "custom/test")
        self.assertEqual(self.git(self.source, "status", "--porcelain").stdout.strip(), "")
        self.assertEqual((self.source / "custom.txt").read_text(), "custom functionality\n")

    def test_merge_preserves_custom_changes_and_source_checkout(self):
        (self.upstream / "new.txt").write_text("new release\n")
        self.commit(self.upstream, "upstream release")
        candidate = self.root / "candidate"
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((candidate / "custom.txt").read_text(), "custom functionality\n")
        self.assertEqual((candidate / "new.txt").read_text(), "new release\n")
        self.assertEqual(json.loads((candidate / "plan/upstream-update/result.json").read_text())["status"], "prepared-not-validated")
        self.unchanged()

    def test_conflict_remains_in_candidate(self):
        (self.source / "feature.txt").write_text("our change\n")
        self.commit(self.source, "custom edit")
        self.before = self.git(self.source, "rev-parse", "HEAD").stdout.strip()
        (self.upstream / "feature.txt").write_text("their change\n")
        self.commit(self.upstream, "upstream edit")
        candidate = self.root / "candidate"
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("UU feature.txt", self.git(candidate, "status", "--porcelain").stdout)
        self.unchanged()

    def test_candidate_records_merged_upstream_base_and_version(self):
        inventory = self.source / "docs/custom/features.json"
        inventory.parent.mkdir(parents=True)
        inventory.write_text(json.dumps({"schema": 1, "upstream_base": "old",
                                         "upstream_version": "v0.4.0", "features": []}))
        self.commit(self.source, "custom inventory")
        self.before = self.git(self.source, "rev-parse", "HEAD").stdout.strip()
        (self.upstream / "Cargo.toml").write_text('[workspace.package]\nversion = "0.5.0"\n')
        self.commit(self.upstream, "next upstream version")
        tip = self.git(self.upstream, "rev-parse", "HEAD").stdout.strip()
        candidate = self.root / "candidate"
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 0, result.stderr)
        merged = json.loads((candidate / "docs/custom/features.json").read_text())
        self.assertEqual(merged["upstream_base"], tip)
        self.assertEqual(merged["upstream_version"], "v0.5.0")
        self.assertEqual(json.loads(inventory.read_text())["upstream_base"], "old")
        self.unchanged()

    def test_existing_candidate_never_overwritten(self):
        candidate = self.root / "candidate"
        candidate.mkdir()
        marker = candidate / "keep.txt"
        marker.write_text("retain me")
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(marker.read_text(), "retain me")
        self.unchanged()

    def test_older_release_does_not_roll_inventory_backward(self):
        older = self.git(self.upstream, "rev-parse", "HEAD").stdout.strip()
        self.git(self.upstream, "tag", "v0.4.0", older)
        (self.upstream / "new.txt").write_text("next release\n")
        self.commit(self.upstream, "next release")
        tip = self.git(self.upstream, "rev-parse", "HEAD").stdout.strip()
        self.git(self.source, "fetch", "upstream", "main")
        self.git(self.source, "merge", "--no-edit", tip)
        inventory = self.source / "docs/custom/features.json"
        inventory.parent.mkdir(parents=True)
        data = {"schema": 1, "upstream_base": tip, "upstream_version": "v0.5.0", "features": []}
        inventory.write_text(json.dumps(data))
        self.commit(self.source, "record integrated release")
        self.before = self.git(self.source, "rev-parse", "HEAD").stdout.strip()
        candidate = self.root / "candidate"
        result = self.invoke("--prepare-only", "--ref", "v0.4.0", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((candidate / "docs/custom/features.json").read_text()), data)
        self.unchanged()

    def test_malformed_inventory_preserves_candidate_and_source(self):
        inventory = self.source / "docs/custom/features.json"
        inventory.parent.mkdir(parents=True)
        inventory.write_text("{invalid inventory")
        self.commit(self.source, "malformed inventory fixture")
        self.before = self.git(self.source, "rev-parse", "HEAD").stdout.strip()
        candidate = self.root / "candidate"
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 3, result.stderr)
        report = json.loads((candidate / "plan/upstream-update/result.json").read_text())
        self.assertEqual(report["status"], "inventory-needs-review")
        self.assertEqual((candidate / "docs/custom/features.json").read_text(), "{invalid inventory")
        self.unchanged()

    def test_dirty_source_and_revision_expressions_refused(self):
        for ref in ("--upload-pack=bad", "main~1", "main..release", "../main", "main@{1}"):
            result = self.invoke("--plan", "--ref", ref)
            self.assertNotEqual(result.returncode, 0, ref)
            self.unchanged()
        (self.source / "custom.txt").write_text("uncommitted work")
        result = self.invoke("--plan")
        self.assertEqual(result.returncode, 1)
        self.assertEqual((self.source / "custom.txt").read_text(), "uncommitted work")

    def test_nested_candidate_refused(self):
        candidate = self.source / "nested-candidate"
        result = self.invoke("--prepare-only", "--worktree", str(candidate))
        self.assertEqual(result.returncode, 1)
        self.assertFalse(candidate.exists())
        self.unchanged()

    def test_main_branch_refused(self):
        self.git(self.source, "switch", "main")
        result = self.invoke("--plan")
        self.assertEqual(result.returncode, 1)
        self.assertIn("protected", result.stderr)

    def test_plan_creates_no_worktree(self):
        result = self.invoke("--plan")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.git(self.source, "worktree", "list", "--porcelain").stdout.split("worktree ")) - 1, 1)
        self.unchanged()

    def test_failed_gate_leaves_merge_uncommitted(self):
        (self.upstream / "new.txt").write_text("new release\n")
        self.commit(self.upstream, "upstream release")
        candidate = self.root / "candidate"
        # The fixture lacks the retention checker, so the first mandatory gate fails.
        result = self.invoke("--worktree", str(candidate))
        self.assertEqual(result.returncode, 3, result.stderr)
        report = json.loads((candidate / "plan/upstream-update/result.json").read_text())
        self.assertEqual(report["status"], "checks-failed")
        self.assertEqual(self.git(candidate, "rev-parse", "HEAD").stdout.strip(), self.before)
        self.git(candidate, "rev-parse", "--verify", "MERGE_HEAD")
        self.assertTrue((candidate / "new.txt").is_file())
        self.unchanged()


if __name__ == "__main__":
    unittest.main()
