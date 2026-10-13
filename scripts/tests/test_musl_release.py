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

    def test_musl_archive_runs_in_isolated_alpine(self):
        """Validate the extracted artifact rather than only the build-tree binary."""
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        job = workflow.split("  musl-verify:\n", 1)[1].split("  publish:\n", 1)[0]
        self.assertIn("tar xzf dist/nulang-linux-x86_64-musl.tar.gz", job)
        self.assertIn("docker run --rm --network none --read-only --cap-drop ALL", job)
        self.assertIn("--security-opt no-new-privileges", job)
        self.assertIn("alpine:3.22", job)
        self.assertIn("--backend native", job)
        self.assertIn("nulang-linux-x86_64-musl", job)

    def test_musl_profile_runs_real_tcp_and_mutual_tls_regressions(self):
        """Existing transport tests should execute against musl, not just compile."""
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        job = workflow.split("  musl-verify:\n", 1)[1].split("  publish:\n", 1)[0]
        self.assertIn("cargo test --locked --release --target x86_64-unknown-linux-musl", job)
        self.assertIn("--no-default-features --features release-musl --lib test_mutual_tls_", job)
        self.assertIn("--no-default-features --features release-musl --lib test_transport_send_receive", job)
        runtime = (ROOT / "src/runtime/tests.rs").read_text(encoding="utf-8")
        for test_name in (
            "test_mutual_tls_connect_and_verify",
            "test_mutual_tls_cluster_converges",
            "test_mutual_tls_rejects_cert_identity_mismatch",
            "test_mutual_tls_rejects_plaintext_peer",
        ):
            self.assertIn("fn " + test_name + "(", runtime)

    def test_cross_libc_interop_is_a_separate_pr_only_gate(self):
        """Build separate GNU and musl probe processes and exercise both directions."""
        with (ROOT / "Cargo.toml").open("rb") as fh:
            manifest = tomllib.load(fh)
        probe = next(
            (ex for ex in manifest.get("example", [])
             if ex.get("name") == "libc_interop_probe"), None
        )
        self.assertIsNotNone(probe)
        self.assertEqual(probe["required-features"], ["tcp", "tls"])
        self.assertTrue((ROOT / probe["path"]).is_file())

        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        job = workflow.split("  musl-verify:\n", 1)[1].split("  publish:\n", 1)[0]
        self.assertIn("--target x86_64-unknown-linux-gnu --no-default-features", job)
        self.assertIn("--target x86_64-unknown-linux-musl --no-default-features", job)
        self.assertIn("--example libc_interop_probe", job)
        self.assertIn("python3 scripts/check_libc_interop.py", job)
        self.assertIn("--gnu target/x86_64-unknown-linux-gnu/release/examples/libc_interop_probe", job)
        self.assertIn("--musl target/x86_64-unknown-linux-musl/release/examples/libc_interop_probe", job)
        self.assertIn("  publish:\n", workflow)

    def test_cross_libc_readiness_rejects_unexpected_addresses_and_ids(self):
        import importlib.util
        script = ROOT / "scripts/check_libc_interop.py"
        spec = importlib.util.spec_from_file_location("libc_interop", script)
        self.assertIsNotNone(spec)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertEqual(
            module.parse_ready("READY 127.0.0.1:30001 42"),
            ("127.0.0.1:30001", "42"),
        )
        for invalid in (
            "", "READY 192.168.1.1:30001 42",
            "READY 127.0.0.1:0 42",
            "READY 127.0.0.1:30001 -1",
            "READY 127.0.0.1:30001 18446744073709551616",
        ):
            with self.subTest(value=invalid):
                with self.assertRaises(RuntimeError):
                    module.parse_ready(invalid)

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
