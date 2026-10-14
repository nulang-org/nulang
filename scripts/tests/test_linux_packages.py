#!/usr/bin/env python3
"""Regression checks for the Linux distro packaging release contract."""

from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/release.yml"
PACKAGER = ROOT / "scripts/package_linux.sh"


class LinuxPackageContractTests(unittest.TestCase):
    def test_release_publishes_both_package_formats_from_linux_matrix(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("scripts/package_linux.sh", workflow)
        self.assertIn("if: runner.os == 'Linux'", workflow)
        self.assertIn("dist/*.deb", workflow)
        self.assertIn("dist/*.rpm", workflow)
        self.assertIn("scripts/tests/test_linux_packages.py", workflow)
        self.assertIn("ubuntu-22.04", workflow, "pin the Linux release glibc baseline")

    def test_architecture_metadata_and_package_contents(self):
        script = PACKAGER.read_text(encoding="utf-8")
        self.assertIn("deb_arch=amd64", script)
        self.assertIn("deb_arch=arm64", script)
        self.assertIn("BuildArch: $arch", script)
        self.assertIn("/usr/bin/nulang", script)
        self.assertIn("--root-owner-group", script)
        self.assertIn("sha256sum", script)
        self.assertIn("readelf -h", script)

    def test_untrusted_version_is_rejected_before_packaging(self):
        for version in ("1.0.0\nMaintainer: attacker", "1.0.0-rc1", "../1.0.0"):
            result = subprocess.run(
                ["bash", str(PACKAGER), "/bin/true", version, "x86_64", "/tmp/nulang-invalid-package"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertIn("unsupported package version", result.stderr)

    def test_binary_architecture_is_checked(self):
        script = PACKAGER.read_text(encoding="utf-8")
        self.assertIn("binary ELF architecture does not match requested", script)


if __name__ == "__main__":
    unittest.main()
