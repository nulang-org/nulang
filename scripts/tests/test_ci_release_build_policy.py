#!/usr/bin/env python3
"""Regression guards for the primary Rust CI cache and release policy."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


class CiReleaseBuildPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")
        cls.release = cls.workflow.split("  release:\n", 1)[1].split("  nula-test:\n", 1)[0]

    def test_cargo_compilation_artifacts_are_stored_where_rust_cache_looks(self):
        # The default rust-cache workspace is ./target, not /tmp/cargo-target.
        self.assertIn("  CARGO_TARGET_DIR: ${{ github.workspace }}/target\n", self.workflow)
        self.assertNotIn("/tmp/cargo-target", self.workflow)
        self.assertIn("      - name: Cache cargo\n        uses: Swatinem/rust-cache@v2", self.release)

    def test_prs_build_and_smoke_test_optimized_cli_without_compiling_release_test_harnesses(self):
        self.assertIn("cargo build --locked --release --no-default-features --features release-cli --bin nulang", self.release)
        self.assertIn("./target/release/nulang --eval", self.release)
        self.assertIn("./target/release/nulang", self.release)
        self.assertIn("      - name: Test (same profile as public release artifacts)\n        if: github.event_name == 'push' && github.ref == 'refs/heads/main'\n", self.release)

    def test_main_keeps_full_optimized_release_tests(self):
        self.assertIn("        run: cargo test --locked --release --no-default-features --features release-cli", self.release)
        self.assertIn("        if: github.event_name == 'push' && github.ref == 'refs/heads/main'", self.release)

    def test_release_binary_checks_and_artifact_upload_use_same_target(self):
        self.assertIn("bash scripts/check_release_linux.sh target/release/nulang", self.release)
        self.assertIn("cp target/release/nulang ci-artifacts/nulang", self.release)
        self.assertIn("      - name: Upload tested release binary", self.release)


if __name__ == "__main__":
    unittest.main()
