#!/usr/bin/env python3
"""Regression: arm64 multiarch must never query amd64-only Ubuntu mirrors."""
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / 'configure_ubuntu_arm64_sources.py'


class AptSourceTests(unittest.TestCase):
    def test_traditional_and_deb822_sources_are_both_restricted(self):
        with tempfile.TemporaryDirectory() as tmp:
            apt = pathlib.Path(tmp)
            (apt / 'sources.list.d').mkdir()
            (apt / 'sources.list').write_text(
                'deb http://archive.ubuntu.com/ubuntu jammy main restricted universe\n'
                'deb https://security.ubuntu.com/ubuntu jammy-security main restricted\n'
                'deb [signed-by=/key.gpg] http://archive.ubuntu.com/ubuntu jammy-updates main\n'
                'deb https://packages.example.org/apt jammy main\n'
            )
            (apt / 'sources.list.d/ubuntu.sources').write_text(
                'Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\nSuites: jammy jammy-updates\nComponents: main\nSigned-By: /key\n\n'
                'Types: deb\nURIs: https://security.ubuntu.com/ubuntu/\nSuites: jammy-security\nComponents: main\n\n'
                'Types: deb\nURIs: https://packages.example.org/debian/\nSuites: jammy\nComponents: main\n'
            )
            self.run_config(apt)
            lines = (apt/'sources.list').read_text().splitlines()
            self.assertIn('deb [arch=amd64] http://archive.ubuntu.com/ubuntu jammy main restricted universe', lines)
            self.assertIn('deb [arch=amd64] https://security.ubuntu.com/ubuntu jammy-security main restricted', lines)
            self.assertIn('deb [signed-by=/key.gpg arch=amd64] http://archive.ubuntu.com/ubuntu jammy-updates main', lines)
            self.assertIn('deb https://packages.example.org/apt jammy main', lines)
            deb822 = (apt/'sources.list.d/ubuntu.sources').read_text()
            self.assertEqual(deb822.count('Architectures: amd64'), 2)
            self.assertIn('URIs: https://packages.example.org/debian/\nSuites: jammy\nComponents: main', deb822)
            arm = (apt/'sources.list.d/nulang-arm64.sources').read_text()
            self.assertIn('Architectures: arm64', arm)
            self.assertIn('ports.ubuntu.com/ubuntu-ports/', arm)
            self.assertIn('jammy-security', arm)
            self.assertNotIn('arm64', (apt/'sources.list').read_text())
            before = {p.relative_to(apt): p.read_text() for p in apt.rglob('*') if p.is_file()}
            self.run_config(apt)
            after = {p.relative_to(apt): p.read_text() for p in apt.rglob('*') if p.is_file()}
            self.assertEqual(before, after)

    def test_fails_closed_for_invalid_codename(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                ['python3', str(SCRIPT), '--apt-dir', tmp, '--codename', 'jammy;rm -rf /'],
                capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((pathlib.Path(tmp) / 'sources.list.d/nulang-arm64.sources').exists())

    def run_config(self, apt):
        subprocess.run(
            ['python3', str(SCRIPT), '--apt-dir', str(apt), '--codename', 'jammy'],
            check=True,
        )


if __name__ == '__main__':
    unittest.main()
