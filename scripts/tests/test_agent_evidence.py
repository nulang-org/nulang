"""Contract tests for the read-only local agent evidence collector."""
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parents[1] / 'agent_evidence.py'
spec = importlib.util.spec_from_file_location('agent_evidence', MODULE_PATH)
agent_evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent_evidence)


def git(cwd, *args):
    return subprocess.run(['git', *args], cwd=cwd, check=True,
                          capture_output=True, text=True).stdout.strip()


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = Path(self.tmp.name)
        git(self.cwd, 'init', '-q')
        git(self.cwd, 'config', 'user.name', 'Test')
        git(self.cwd, 'config', 'user.email', 'test@example.invalid')
        (self.cwd / 'readme.txt').write_text('before\n')
        git(self.cwd, 'add', '.')
        git(self.cwd, 'commit', '-qm', 'initial')
        self.base = git(self.cwd, 'rev-parse', 'HEAD')
        (self.cwd / 'readme.txt').write_text('after\n')
        git(self.cwd, 'add', '.')
        git(self.cwd, 'commit', '-qm', 'change')
        self.head = git(self.cwd, 'rev-parse', 'HEAD')

    def test_collects_bounded_execution_results_without_logs(self):
        out = agent_evidence.collect(self.cwd, self.base, [('focused', ['python3', '-c', 'print("SECRET_VALUE")'])])
        self.assertEqual(out['head_sha'], self.head)
        self.assertEqual(out['changed_files'], ['readme.txt'])
        self.assertEqual(out['checks'][0]['status'], 'passed')
        self.assertEqual(out['ci']['status'], 'unknown')
        self.assertNotIn('SECRET_VALUE', json.dumps(out))

    def test_failed_check_stays_failed_and_never_claims_ready(self):
        out = agent_evidence.collect(self.cwd, self.base, [('bad', ['python3', '-c', 'exit(3)'])])
        self.assertEqual(out['checks'][0]['exit_code'], 3)
        self.assertEqual(out['checks'][0]['status'], 'failed')
        self.assertFalse(out['ready_for_review'])

    def test_unknown_check_and_dirty_tree_not_ready(self):
        out = agent_evidence.collect(self.cwd, self.base, [])
        self.assertFalse(out['ready_for_review'])
        (self.cwd / 'dirty.txt').write_text('not committed\n')
        out = agent_evidence.collect(self.cwd, self.base, [('focused', ['python3', '-c', 'pass'])])
        self.assertTrue(out['dirty'])
        self.assertFalse(out['ready_for_review'])

    def test_invalid_base_rejected_without_running_checks(self):
        with self.assertRaisesRegex(ValueError, 'base SHA'):
            agent_evidence.collect(self.cwd, 'main', [('bad', ['python3', '-c', 'exit(3)'])])


if __name__ == '__main__':
    unittest.main()
