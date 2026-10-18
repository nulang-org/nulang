import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / 'diagnostic_baseline.py'


def diagnostic(code='W0101', message='prefer explicit effect', file='src/main.nula', severity='warning'):
    return {'code': code, 'severity': severity, 'message': message,
            'span': {'file': file, 'line': 3, 'col': 2, 'end_line': 3, 'end_col': 5},
            'notes': [], 'suggestion': None}


class DiagnosticBaselineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.report = self.root / 'report.json'
        self.baseline = self.root / 'baseline.json'

    def run_gate(self, *extra):
        return subprocess.run([sys.executable, str(SCRIPT), '--report', str(self.report),
                               '--baseline', str(self.baseline), '--root', str(self.root), *extra],
                              capture_output=True, text=True)

    def write_report(self, diagnostics):
        self.report.write_text(json.dumps({'schema_version': 1, 'command': 'check',
                                           'file': 'src/main.nula', 'ok': not any(
                                               d['severity'] == 'error' for d in diagnostics),
                                           'diagnostics': diagnostics}))

    def test_update_and_check_baseline_ignores_position_changes(self):
        self.write_report([diagnostic()])
        self.assertEqual(self.run_gate('--update').returncode, 0)
        entry = json.loads(self.baseline.read_text())['warnings'][0]
        self.assertEqual(entry, {'file': 'src/main.nula', 'code': 'W0101',
                                 'message': 'prefer explicit effect', 'count': 1})
        moved = diagnostic()
        moved['span']['line'] = 200
        self.write_report([moved])
        self.assertEqual(self.run_gate().returncode, 0)

    def test_new_warning_fails_check(self):
        self.write_report([diagnostic()])
        self.assertEqual(self.run_gate('--update').returncode, 0)
        self.write_report([diagnostic(), diagnostic('W0102', 'new warning')])
        result = self.run_gate()
        self.assertEqual(result.returncode, 1)
        self.assertIn('new warning', result.stderr)

    def test_stale_baseline_fails_check(self):
        self.write_report([diagnostic()])
        self.assertEqual(self.run_gate('--update').returncode, 0)
        self.write_report([])
        self.assertEqual(self.run_gate().returncode, 1)

    def test_duplicate_count_is_not_silently_suppressed(self):
        self.write_report([diagnostic()])
        self.assertEqual(self.run_gate('--update').returncode, 0)
        self.write_report([diagnostic(), diagnostic()])
        self.assertEqual(self.run_gate().returncode, 1)

    def test_errors_are_never_baselined_or_accepted(self):
        self.write_report([diagnostic(code='E0301', message='capability violation', severity='error')])
        self.assertEqual(self.run_gate('--update').returncode, 1)
        self.assertFalse(self.baseline.exists())
        self.assertEqual(self.run_gate().returncode, 1)

    def test_uncoded_warnings_are_not_suppressible(self):
        self.write_report([diagnostic(code=None)])
        self.assertEqual(self.run_gate('--update').returncode, 2)
        self.assertFalse(self.baseline.exists())

    def test_rejects_invalid_report_and_baseline_schema(self):
        self.report.write_text('{not json}')
        self.assertEqual(self.run_gate('--update').returncode, 2)
        self.write_report([])
        self.baseline.write_text('{"schema_version": 99, "warnings": []}')
        self.assertEqual(self.run_gate().returncode, 2)

    def test_missing_baseline_rejects_warnings_but_allows_clean_report(self):
        self.write_report([])
        self.assertEqual(self.run_gate().returncode, 0)
        self.write_report([diagnostic()])
        self.assertEqual(self.run_gate().returncode, 1)


    def test_relative_symlink_outside_root_is_rejected(self):
        outside = self.root.parent / (self.root.name + '-outside.nula')
        outside.write_text('external source', encoding='utf-8')
        self.addCleanup(outside.unlink)
        try:
            (self.root / 'external.nula').symlink_to(outside)
        except (OSError, NotImplementedError) as exc:
            self.skipTest(f'file symlinks unavailable on this runner: {exc}')
        self.write_report([diagnostic(file='external.nula')])
        result = self.run_gate('--update')
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn('outside --root', result.stderr)
        self.assertFalse(self.baseline.exists())

    def test_relative_symlink_inside_root_is_canonicalized(self):
        source_dir = self.root / 'src'
        source_dir.mkdir()
        (source_dir / 'main.nula').write_text('let n = 1', encoding='utf-8')
        try:
            (self.root / 'alias.nula').symlink_to(source_dir / 'main.nula')
        except (OSError, NotImplementedError) as exc:
            self.skipTest(f'file symlinks unavailable on this runner: {exc}')
        self.write_report([diagnostic(file='alias.nula')])
        result = self.run_gate('--update')
        self.assertEqual(result.returncode, 0, result.stderr)
        entries = json.loads(self.baseline.read_text(encoding='utf-8'))['warnings']
        self.assertEqual(entries[0]['file'], 'src/main.nula')


if __name__ == '__main__':
    unittest.main()
