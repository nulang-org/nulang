"""Portable and accelerated Nulang contributor environment checks."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

DOCTOR = pathlib.Path(__file__).resolve().parents[1] / 'dev-doctor.sh'


class DeveloperDoctorTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = pathlib.Path(self.tmp.name) / 'checkout'
        (self.repo / 'scripts').mkdir(parents=True)
        shutil.copyfile(DOCTOR, self.repo / 'scripts' / 'dev-doctor.sh')
        (self.repo / 'rust-toolchain.toml').write_text('[toolchain]\nchannel = "1.95.0"\n')
        self.bin = pathlib.Path(self.tmp.name) / 'bin'
        self.bin.mkdir()
        for command in ('git', 'rustup', 'rustc', 'cargo', 'python3', 'cc', 'pkg-config'):
            self.stub(command)

    def stub(self, name, body='exit 0'):
        path = self.bin / name
        if name == 'rustc':
            body = 'printf "rustc 1.95.0 (test)\\n"' if body == 'exit 0' else body
        path.write_text('#!/bin/sh\n' + body + '\n')
        path.chmod(0o755)

    def run_doctor(self, *args):
        env = os.environ.copy()
        env['PATH'] = str(self.bin) + os.pathsep + '/usr/bin:/bin'
        env.pop('CI', None)
        return subprocess.run(
            ['bash', str(self.repo / 'scripts' / 'dev-doctor.sh'), *args],
            text=True, capture_output=True, env=env, cwd=self.repo, check=False,
        )

    def test_portable_environment_does_not_require_mise_or_mbx(self):
        result = self.run_doctor()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('doctor passed', result.stdout)
        self.assertNotIn('missing required command: mbx', result.stderr)
        self.assertNotIn('missing required command: mise', result.stderr)

    def test_required_native_dependency_still_fails(self):
        (self.bin / 'cargo').unlink()
        result = self.run_doctor()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing required command: cargo', result.stderr)

    def test_accelerated_profile_requires_mise_and_mbx(self):
        result = self.run_doctor('--accelerated')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing required command: mise', result.stderr)
        self.assertIn('missing required command: mbx', result.stderr)

    def test_accelerated_profile_passes_with_healthy_tools(self):
        self.stub('mise')
        self.stub('mbx')
        result = self.run_doctor('--accelerated')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('mise doctor', result.stdout)

    def test_mise_health_is_warning_in_portable_profile_but_error_if_requested(self):
        self.stub('mise', 'exit 1')
        portable = self.run_doctor()
        self.assertEqual(portable.returncode, 0, portable.stdout + portable.stderr)
        self.assertIn('mise doctor reported a problem', portable.stderr)
        self.stub('mbx')
        accelerated = self.run_doctor('--accelerated')
        self.assertNotEqual(accelerated.returncode, 0)
        self.assertIn('mise doctor reported a problem', accelerated.stderr)

    def test_unknown_arguments_fail_with_usage(self):
        result = self.run_doctor('--not-a-profile')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Usage:', result.stderr)


if __name__ == '__main__':
    unittest.main()
