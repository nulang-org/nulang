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

    def test_native_arm64_and_clean_distro_installations_gate_publication(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("  linux-install-smoke:\n", workflow)
        self.assertIn("ubuntu-22.04-arm", workflow)
        self.assertIn("debian:12", workflow)
        self.assertIn("fedora:44", workflow)
        self.assertIn("    needs: [release, linux-install-smoke]", workflow)
        self.assertIn("sudo apt install", (ROOT / "docs/src/content/docs/getting-started/installation.mdx").read_text(encoding="utf-8"))
        self.assertIn("docker run --rm", workflow)
        self.assertIn("apt-get remove -y nulang", workflow)
        self.assertIn("dnf remove -y nulang", workflow)

    def test_linux_package_version_does_not_require_python_311(self):
        # ubuntu-22.04 ships Python 3.10 by default; tomllib is 3.11+.
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("cargo metadata --locked --no-deps --format-version 1", workflow)
        self.assertNotIn('version="$(python3 -c \'import tomllib', workflow)

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
