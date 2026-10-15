"""Tests for the bootstrap differential runner's failure-closed contract."""
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / 'bootstrap' / 'verify_differential.py'


class DifferentialContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('bootstrap_differential', SCRIPT)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_normalization_removes_comments_only(self):
        self.assertEqual(self.module.instructions('; marker\n04080000\n; closure\n12080000\n'), '0408000012080000')

    def test_normalization_rejects_empty_emission(self):
        with self.assertRaisesRegex(ValueError, 'empty'):
            self.module.instructions('; only remarks\n')

    def test_corpus_rejects_duplicate_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'corpus.json'
            path.write_text(json.dumps([{'id': 'x', 'source': '1', 'expected': '1'}] * 2))
            with self.assertRaisesRegex(ValueError, 'duplicate'):
                self.module.load_cases(path)

    def test_corpus_rejects_missing_expected_outcome(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'corpus.json'
            path.write_text(json.dumps([{'id': 'x', 'source': '1'}]))
            with self.assertRaisesRegex(ValueError, 'expected'):
                self.module.load_cases(path)

    def test_corpus_rejects_unsafe_fixture_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'corpus.json'
            path.write_text(json.dumps([{'id': '../escape', 'source': '1', 'expected': '1'}]))
            with self.assertRaisesRegex(ValueError, 'id'):
                self.module.load_cases(path)

    def test_process_failures_are_not_ignored(self):
        with self.assertRaisesRegex(RuntimeError, 'exited 3'):
            self.module.run([sys.executable, '-c', 'import sys; sys.exit(3)'])

    def test_result_rejects_empty_vm_output(self):
        with self.assertRaisesRegex(ValueError, 'empty'):
            self.module.last_result(' \n')

    def test_result_uses_final_nonempty_line(self):
        self.assertEqual(self.module.last_result('booted\n42\n'), '42')


if __name__ == '__main__':
    unittest.main()
