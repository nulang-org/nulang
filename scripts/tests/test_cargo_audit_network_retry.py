"""Integration tests for the fail-closed cargo-audit network retry wrapper."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'ci' / 'cargo-audit-network-retry.py'

class AuditRetryTests(unittest.TestCase):
    def check_results(self, planned, expected_status, expected_calls):
        with tempfile.TemporaryDirectory(prefix='nulang-audit-test-') as directory:
            root = Path(directory)
            fake = root / 'cargo'
            fake.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
assert sys.argv[1:] == ['audit'], sys.argv
path = Path(os.environ['FAKE_AUDIT_CALLS'])
count = int(path.read_text()) if path.exists() else 0
path.write_text(str(count + 1))
planned = json.loads(os.environ['FAKE_AUDIT_RESULTS'])
message, code = planned[min(count, len(planned)-1)]
print(message)
sys.exit(code)
''')
            fake.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                       FAKE_AUDIT_CALLS=str(root / 'calls'),
                       FAKE_AUDIT_RESULTS=json.dumps(planned))
            result = subprocess.run([sys.executable, str(SCRIPT)],
                                    text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, env=env, timeout=18)
            self.assertEqual(result.returncode, expected_status, result.stdout)
            self.assertEqual(int((root / 'calls').read_text()), expected_calls, result.stdout)
            return result.stdout

    def test_success_runs_once(self):
        self.check_results([['Scanning Cargo.lock: 0 vulnerabilities', 0]], 0, 1)

    def test_transient_advisory_database_download_retries_and_then_passes(self):
        output = self.check_results([
            ["error: couldn't fetch advisory database: git operation failed", 1],
            ['Scanning Cargo.lock: 0 vulnerabilities', 0]], 0, 2)
        self.assertIn('0 vulnerabilities', output)

    def test_repeated_fetch_failure_stays_blocking_after_bounded_attempts(self):
        self.check_results([
            ["error: couldn't fetch advisory database: git operation failed", 1]], 1, 3)

    def test_real_vulnerability_stays_blocking_without_retry(self):
        self.check_results([['error: 1 vulnerability found in Cargo.lock', 1]], 1, 1)

    def test_unknown_failure_stays_blocking_without_retry(self):
        self.check_results([['error: invalid Cargo.lock', 2]], 2, 1)

if __name__ == '__main__':
    unittest.main()
