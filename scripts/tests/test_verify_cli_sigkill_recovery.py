"""Contract tests for the controlled SIGKILL recovery harness (no Nulang build)."""
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from verify_cli_sigkill_recovery import verify, RecoveryError, parse_ready

FAKE = '''#!/usr/bin/env python3
import os, pathlib, sys
store = pathlib.Path(next(v for v in sys.argv if v.startswith('json:'))[5:])
store.mkdir(parents=True, exist_ok=True)
snapshot = store / 'actor_1' / 'snapshot.json'
snapshot.parent.mkdir(exist_ok=True)
old = int(snapshot.read_text()) if snapshot.exists() else 0
if os.environ.get('FAKE_NO_RESTORE') == '1':
    old = 0
if os.environ.get('FAKE_EXIT_EARLY') == '1':
    sys.exit(0)
if os.environ.get('FAKE_NO_SAVE') != '1':
    snapshot.write_text(str(old + 1))
print(f'RESTORED={old} SAVED={old + 1}', flush=True)
sys.stdin.readline()
'''


@unittest.skipUnless(os.name == "posix", "requires POSIX SIGKILL")
class SigkillHarnessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.binary = Path(self.tmp.name) / 'fake-cli'
        self.binary.write_text(FAKE)
        self.binary.chmod(0o755)
        old_env = os.environ.copy()
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(old_env)))
        for key in ['FAKE_NO_RESTORE', 'FAKE_EXIT_EARLY', 'FAKE_NO_SAVE']:
            os.environ.pop(key, None)

    def test_accepts_stable_output(self):
        self.assertEqual(parse_ready('RESTORED=0 SAVED=1'), (0, 1))

    def test_rejects_unexpected_output(self):
        with self.assertRaises(RecoveryError):
            parse_ready('RESTORED=0 SAVED=nan')

    def test_harness_proves_killed_process_state_was_restored(self):
        verify(self.binary, timeout=5)

    def test_fails_if_first_process_exits_without_reaching_checkpoint(self):
        os.environ['FAKE_EXIT_EARLY'] = '1'
        with self.assertRaises(RecoveryError):
            verify(self.binary, timeout=3)

    def test_fails_without_durable_snapshot(self):
        os.environ['FAKE_NO_SAVE'] = '1'
        with self.assertRaises(RecoveryError):
            verify(self.binary, timeout=3)

    def test_fails_if_second_process_drops_persisted_state(self):
        os.environ['FAKE_NO_RESTORE'] = '1'
        with self.assertRaises(RecoveryError):
            verify(self.binary, timeout=3)


if __name__ == '__main__':
    unittest.main()
