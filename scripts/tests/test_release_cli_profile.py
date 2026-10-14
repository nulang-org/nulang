#!/usr/bin/env python3
"""Guard release feature parity: public CLI differs from full only by Python."""

import pathlib
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class ReleaseCliProfileTests(unittest.TestCase):
    def test_public_release_keeps_all_non_python_full_features(self):
        with (ROOT / "Cargo.toml").open("rb") as source:
            features = tomllib.load(source)["features"]

        full = set(features["full"])
        self.assertIn("python", full, "the full profile must retain Python interop")
        self.assertIn("release-cli", features, "public release feature profile is missing")
        public = set(features["release-cli"])
        self.assertNotIn("python", public, "release-cli must exclude PyO3")
        self.assertEqual(
            public,
            full - {"python"},
            "release-cli must match every non-Python full feature exactly",
        )


    def test_pull_request_release_rehearsal_never_publishes(self):
        """PR builds may exercise every target, but publishing remains tag-only."""
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertIn(
            "  pull_request:\n",
            workflow,
            "the four-platform release matrix must be testable before creating a tag",
        )
        self.assertIn(
            "      - name: Verify tag matches crate and CLI versions\n"
            "        if: github.event_name == 'push'\n",
            workflow,
            "a PR has no release tag to verify",
        )
        self.assertIn(
            "  publish:\n"
            "    name: Publish Release\n"
            "    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')\n",
            workflow,
            "pull requests must never have a path to publishing a GitHub Release",
        )


    def test_linux_arm64_release_runs_natively_before_publication(self):
        """Native ARM64 avoids multiarch apt mirrors and validates the real CPU."""
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertIn(
            "os: ubuntu-22.04-arm\n            target: aarch64-unknown-linux-gnu",
            workflow,
        )
        self.assertIn("      - name: Install native Linux build dependencies", workflow)
        self.assertIn("test \"$(uname -m)\" = \"${{ matrix.package_arch }}\"", workflow)
        self.assertNotIn("sudo dpkg --add-architecture arm64", workflow)
        self.assertNotIn("aarch64-linux-gnu-gcc", workflow)
        self.assertIn("  linux-install-smoke:\n", workflow)
        self.assertIn("    needs: [release, linux-install-smoke]", workflow)


if __name__ == "__main__":
    unittest.main()
