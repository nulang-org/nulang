"""Behavioral regressions for repository worktree and Git-hook entrypoints.

Runs against an isolated local Git remote and stub Cargo/mise commands.
No network, Rust compilation, or external Git hosting is required.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


def run(args, *, cwd=None, env=None):
    return subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)


class WorktreeSetupTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("git"), "Git required")
    def test_creates_worktree_and_runs_setup_using_isolated_local_origin(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            seed = root / "source"
            seed.mkdir()
            for args in (["git", "init", "-q", "-b", "main"],
                         ["git", "config", "user.email", "test@example.org"],
                         ["git", "config", "user.name", "Test"]):
                result = run(args, cwd=seed)
                self.assertEqual(result.returncode, 0, result.stderr)
            (seed / "README.md").write_text("fixture\n")
            for args in (["git", "add", "README.md"], ["git", "commit", "-qm", "seed"]):
                result = run(args, cwd=seed)
                self.assertEqual(result.returncode, 0, result.stderr)
            remote = root / "remote.git"
            self.assertEqual(run(["git", "init", "--bare", "-q", str(remote)]).returncode, 0)
            self.assertEqual(run(["git", "remote", "add", "origin", str(remote)], cwd=seed).returncode, 0)
            self.assertEqual(run(["git", "push", "-q", "-u", "origin", "main"], cwd=seed).returncode, 0)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "mise").write_text("#!/bin/sh\ntest \"$1\" = run && test \"$2\" = setup && touch .mise-setup-ran\n")
            (bin_dir / "mise").chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
            env["NULANG_WORKTREE_ROOT"] = str(root / "worktrees")
            result = run(["bash", str(ROOT / "scripts/worktree-new.sh"), "safe-worktree"], cwd=seed, env=env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            worktree = root / "worktrees" / "source" / "safe-worktree"
            self.assertEqual(run(["git", "branch", "--show-current"], cwd=worktree).stdout.strip(), "agent/safe-worktree")
            self.assertTrue((worktree / ".mise-setup-ran").is_file())


class HookFailurePropagationTests(unittest.TestCase):
    def _run_hook(self, filename, fake_cargo):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "cargo"
            path.write_text("#!/bin/sh\n" + fake_cargo + "\n")
            path.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = tmp + os.pathsep + env["PATH"]
            env["HOME"] = tmp
            home_cargo = Path(tmp) / ".cargo" / "bin"
            home_cargo.mkdir(parents=True)
            (home_cargo / "cargo").symlink_to(path)
            return run(["sh", str(ROOT / ".husky" / filename)], env=env)

    def test_pre_commit_propagates_compiler_failure_with_no_warnings(self):
        result = self._run_hook(
            "pre-commit",
            'if [ "$1" = check ]; then echo "error: deliberately failed" >&2; exit 17; fi; exit 0',
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_pre_push_enforces_blocking_clippy_correctness(self):
        result = self._run_hook(
            "pre-push",
            'if [ "$1" = clippy ]; then case " $* " in *"-D clippy::correctness"*) exit 23;; esac; fi; exit 0',
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
