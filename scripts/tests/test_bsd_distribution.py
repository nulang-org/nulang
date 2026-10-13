#!/usr/bin/env python3
"""Static distribution invariants; native BSD execution is verified in CI VMs."""

import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class BsdDistributionTests(unittest.TestCase):
    def test_freebsd_release_builds_and_smokes_native_cli(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertIn("uses: vmactions/freebsd-vm@v1", workflow)
        self.assertIn("nulang-freebsd-x86_64", workflow)
        self.assertIn("--no-default-features --features release-cli --bin nulang", workflow)
        self.assertIn("examples/01_hello.nula", workflow)
        self.assertIn("nulang-freebsd-x86_64.tar.gz.sha256", workflow)
        self.assertIn("needs: [release, freebsd]", workflow)

    def test_other_bsd_dialects_have_native_portability_smokes(self):
        workflow = (ROOT / ".github/workflows/bsd.yml").read_text(encoding="utf-8")
        for os_name in ("openbsd", "netbsd"):
            with self.subTest(os=os_name):
                self.assertIn(f"vmactions/{os_name}-vm@v1", workflow)
        self.assertIn("--no-default-features --bin nulang", workflow)
        self.assertIn("examples/01_hello.nula", workflow)

    def test_download_choices_match_packaged_release_artifacts(self):
        component = (ROOT / "docs/src/components/PlatformDownloads.astro").read_text(
            encoding="utf-8"
        )
        for artifact in (
            "nulang-linux-x86_64",
            "nulang-linux-aarch64",
            "nulang-macos-aarch64",
            "nulang-windows-x86_64",
            "nulang-freebsd-x86_64",
        ):
            with self.subTest(artifact=artifact):
                self.assertIn(artifact, component)
        self.assertIn(".tar.gz.sha256", component)
        installation = (
            ROOT / "docs/src/content/docs/getting-started/installation.mdx"
        ).read_text(encoding="utf-8")
        self.assertIn("<PlatformDownloads />", installation)
        self.assertIn("OpenBSD", installation)
        self.assertIn("NetBSD", installation)


if __name__ == "__main__":
    unittest.main()
