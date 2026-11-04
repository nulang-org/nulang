"""Guard Cargo build directory consistency across development and release CI."""
from pathlib import Path
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]


class CargoTargetLocationTest(unittest.TestCase):
    def test_default_target_is_disk_backed_worktree_directory(self):
        config = tomllib.loads((ROOT / ".cargo/config.toml").read_text())
        self.assertEqual(config["build"]["target-dir"], "target")

    def test_release_job_uses_consistent_target_directory(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("\n  release:\n", ci)
        job = ci.split("\n  release:\n", 1)[1].split("\n  nula-test:\n", 1)[0]
        self.assertIn("CARGO_TARGET_DIR: target", job)
        self.assertIn("scripts/check_release_linux.sh target/release/nulang", job)
        self.assertIn("cp target/release/nulang ci-artifacts/nulang", job)
        self.assertNotIn("/tmp/cargo-target", job)

    def test_cross_platform_release_keeps_explicit_target(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("CARGO_TARGET_DIR: target", release)


if __name__ == "__main__":
    unittest.main()
