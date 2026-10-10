"""Regression checks for the nightly fuzz log artifact boundary.

Does not require Cargo, GitHub Actions, or a GitHub service token.
"""
from pathlib import Path
import os
import re
import subprocess
import tempfile
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / '.github/workflows/fuzz-nightly.yml'


def _workflow_step(source: str, name: str) -> str:
    pattern = re.compile(r'^      - name: ' + re.escape(name) + r'\s*$', re.M)
    matched = pattern.search(source)
    if not matched:
        raise AssertionError(f'missing workflow step: {name}')
    remainder = source[matched.end():]
    end = re.search(r'^      - name: ', remainder, re.M)
    return remainder[:end.start()] if end else remainder


def _script_from_step(step: str) -> str:
    match = re.search(r'^        run: \|\s*$', step, re.M)
    if not match:
        raise AssertionError('expected a multiline bash run block')
    lines = []
    for line in step[match.end():].splitlines():
        if line and not line.startswith('          '):
            break
        lines.append(line[10:] if line else '')
    script = '\n'.join(lines).strip()
    if not script:
        raise AssertionError('empty fuzz runner script')
    return script


class NightlyFuzzLogCaptureContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = WORKFLOW.read_text(encoding='utf-8')
        cls.runner = _workflow_step(cls.source, 'Run differential fuzz shard')
        cls.upload = _workflow_step(cls.source, 'Upload divergence log on failure')

    def test_failure_artifact_path_matches_runner_and_is_fail_closed(self):
        self.assertIn('id: fuzz', self.runner)
        self.assertIn('shell: bash', self.runner)
        self.assertIn('set -o pipefail', self.runner)
        self.assertIn('"${RUNNER_TEMP}/fuzz-divergence-shard-${NULANG_FUZZ_SHARD_ID}.log"', self.runner)
        self.assertIn("if: ${{ failure() && steps.fuzz.outcome == 'failure' }}", self.upload)
        self.assertIn('path: ${{ runner.temp }}/fuzz-divergence-shard-${{ matrix.shard }}.log', self.upload)
        self.assertIn('if-no-files-found: error', self.upload)
        self.assertNotIn('/tmp/*.log', self.upload)

    def test_failed_shard_preserves_failure_and_exact_console_output(self):
        script = _script_from_step(self.runner)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            cargo = bin_dir / 'cargo'
            cargo.write_text('#!/bin/sh\nprintf "DIVERGENCE seed=42\\n"\nexit 17\n', encoding='utf-8')
            cargo.chmod(0o755)
            env = os.environ.copy()
            env['PATH'] = f'{bin_dir}:{env.get("PATH", "")}'
            env['RUNNER_TEMP'] = tmp
            env['NULANG_FUZZ_SHARD_ID'] = '7'
            env['NULANG_FUZZ_ITERATIONS'] = '100000'
            proc = subprocess.run(['bash', '-e', '-c', script], cwd=root, env=env,
                                  text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertEqual(proc.returncode, 17, proc.stdout)
            self.assertIn('DIVERGENCE seed=42', proc.stdout)
            logs = sorted(root.glob('fuzz-divergence-shard-*.log'))
            self.assertEqual([p.name for p in logs], ['fuzz-divergence-shard-7.log'])
            self.assertEqual(logs[0].read_text(encoding='utf-8'), 'DIVERGENCE seed=42\n')

    def test_successful_shard_keeps_zero_exit_status(self):
        script = _script_from_step(self.runner)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / 'bin'
            bin_dir.mkdir()
            cargo = bin_dir / 'cargo'
            cargo.write_text('#!/bin/sh\nprintf "OK seed=9\\n"\nexit 0\n', encoding='utf-8')
            cargo.chmod(0o755)
            env = os.environ.copy()
            env['PATH'] = f'{bin_dir}:{env.get("PATH", "")}'
            env['RUNNER_TEMP'] = tmp
            env['NULANG_FUZZ_SHARD_ID'] = '2'
            env['NULANG_FUZZ_ITERATIONS'] = '100000'
            proc = subprocess.run(['bash', '-e', '-c', script], cwd=root, env=env,
                                  text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertEqual(proc.returncode, 0, proc.stdout)
            self.assertEqual((root / 'fuzz-divergence-shard-2.log').read_text(encoding='utf-8'), 'OK seed=9\n')


if __name__ == '__main__':
    unittest.main()
