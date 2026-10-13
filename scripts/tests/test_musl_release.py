#!/usr/bin/env python3
"""Smoke the static ELF gate and check isolation of the experimental musl profile."""

from __future__ import annotations

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class MuslReleaseTests(unittest.TestCase):
    def test_musl_profile_is_an_explicit_subset_of_public_cli(self):
        with (ROOT / 'Cargo.toml').open('rb') as f:
            features = tomllib.load(f)['features']
        portable = set(features['release-musl'])
        self.assertEqual(portable, {'native-codegen', 'tcp', 'tls'})
        self.assertLess(portable, set(features['release-cli']))
        self.assertFalse(portable.intersection({'python', 'ffi', 'sqlite', 'ai-runtime', 'ureq'}))

    def test_musl_preview_is_not_published_as_a_tagged_release(self):
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        self.assertIn('  musl-verify:\n', workflow)
        job = workflow.split('  musl-verify:\n', 1)[1].split('  publish:\n', 1)[0]
        self.assertIn('x86_64-unknown-linux-musl', job)
        self.assertIn('--no-default-features --features release-musl', job)
        self.assertIn('scripts/check_release_linux_static.sh', job)
        self.assertIn('experimental-static-musl-x86_64', job)
        self.assertNotIn('name: nulang-', job)
        publish = workflow.split('  publish:\n', 1)[1]
        self.assertIn('needs: release', publish)
        self.assertNotIn('needs: [release, musl-verify]', publish)

    @unittest.skipUnless(
        sys.platform.startswith('linux') and shutil.which('gcc') and shutil.which('readelf'),
        'requires Linux, gcc, and binutils',
    )
    def test_checker_accepts_static_elf_rejects_dynamic_elf_and_non_elf(self):
        checker = ROOT / 'scripts/check_release_linux_static.sh'
        with tempfile.TemporaryDirectory() as d:
            tmp = pathlib.Path(d)
            src = tmp / 'hello.c'
            src.write_text('int main(void) { return 0; }\n')
            static = tmp / 'static'
            dynamic = tmp / 'dynamic'
            compile_static = subprocess.run(['gcc', '-static', str(src), '-o', str(static)], capture_output=True, text=True)
            if compile_static.returncode:
                self.skipTest('static libc development files unavailable')
            subprocess.run(['gcc', str(src), '-o', str(dynamic)], check=True)
            for candidate, expected in [(static, 0), (dynamic, 1), (src, 1)]:
                result = subprocess.run(['bash', str(checker), str(candidate)], capture_output=True, text=True)
                self.assertEqual(result.returncode, expected, (candidate.name, result.stderr))


    @unittest.skipUnless(
        sys.platform.startswith('linux') and shutil.which('gcc') and shutil.which('readelf'),
        'requires Linux, gcc, and binutils',
    )
    def test_checker_fails_closed_when_readelf_inspection_fails(self):
        """An unreadable program/dynamic header must not count as static."""
        checker = ROOT / 'scripts/check_release_linux_static.sh'
        with tempfile.TemporaryDirectory() as d:
            tmp = pathlib.Path(d)
            src = tmp / 'hello.c'
            src.write_text('int main(void) { return 0; }\n')
            binary = tmp / 'binary'
            subprocess.run(['gcc', str(src), '-o', str(binary)], check=True)
            readelf = tmp / 'readelf'
            real_readelf = shutil.which('readelf')
            readelf.write_text(
                '#!/bin/sh\n'
                'if [ "$1" = "-h" ]; then exec "' + real_readelf + '" "$@"; fi\n'
                'exit 19\n'
            )
            readelf.chmod(0o755)
            env = {**os.environ, 'PATH': str(tmp) + os.pathsep + os.environ['PATH']}
            result = subprocess.run(['bash', str(checker), str(binary)], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

if __name__ == '__main__':
    unittest.main()
