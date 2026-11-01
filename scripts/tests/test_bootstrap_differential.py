"""Tests for the bootstrap differential runner's failure-closed contract."""
import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

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

    def test_fixed_point_verification_keeps_workspace_alive(self):
        with tempfile.TemporaryDirectory() as tmp:
            corpus = pathlib.Path(tmp) / 'corpus.json'
            corpus.write_text(json.dumps([{'id': 'seven', 'source': '7', 'expected': '7'}]))
            inspected = []

            def check_workspace(_nulang, _prepared, _stage2, work, stage1_hex=None):
                self.assertEqual(stage1_hex, b'7\n')
                self.assertTrue(work.is_dir(), 'Stage 3 attempted after workspace cleanup')
                self.assertTrue((work / 'self_compile.nbc').is_file())
                inspected.append(work)

            with mock.patch.object(self.module, 'run', return_value=b'7\n'):
                with mock.patch.object(self.module, 'assemble', return_value=b'fake .nbc'):
                    with mock.patch.object(self.module, 'verify_fixed_point', side_effect=check_workspace):
                        self.module.verify(pathlib.Path('/example/nulang'), corpus, fixed_point=True)
            self.assertEqual(len(inspected), 1)
            self.assertFalse(inspected[0].exists(), 'workspace should be cleaned after verification')

    def test_first_difference_detects_changed_and_missing_bytes(self):
        self.assertEqual(self.module.first_difference(b'ABCD', b'ABXD'), 2)
        self.assertEqual(self.module.first_difference(b'ABCD', b'AB'), 2)
        self.assertEqual(self.module.first_difference(b'ABC', b'ABC'), -1)

    def test_marker_context_identifies_branch_boundaries_before_word(self):
        raw = '; if\n00000001\n; JmpF -> else\n00000002\n; else:\n00000003\n'
        self.assertEqual(
            self.module.marker_context(raw, 2),
            '@0 ; if | @1 ; JmpF -> else | @2 ; else:',
        )

    def test_compiler_source_omits_unused_instr_helper(self):
        # A dead helper still becomes closure bytecode in the self-host chain.
        # The full Stage 2/Stage 3 executable oracle remains authoritative.
        source = (self.module.ROOT / 'bootstrap/compile_hex.nula').read_text()
        self.assertNotIn('fn instr(', source)

    def test_emission_context_shows_numbered_words_near_divergence(self):
        self.assertEqual(
            self.module.emission_context('11111111222222223333333344444444', 2, radius=1),
            '1:22222222 2:[33333333] 3:44444444',
        )
        self.assertEqual(
            self.module.emission_context('11111111', 1, radius=1),
            '0:11111111 1:[<eof>]',
        )

    def test_fixed_point_mismatch_identifies_first_emitted_word(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(self.module, 'run', return_value=b'; comment\n00112244\n'):
                with mock.patch.object(self.module, 'assemble', return_value=b'ABCE'):
                    with self.assertRaisesRegex(AssertionError, 'first differing compiled byte at offset 3') as error:
                        self.module.verify_fixed_point(
                            pathlib.Path('/example/nulang'), b'prepared compiler',
                            b'ABCD', pathlib.Path(tmp),
                            stage1_hex=b'; comment\n00112233\n'
                        )
            self.assertIn('first differing emitted hex character at offset 6', str(error.exception))
            self.assertIn('Stage 1 word=00112233', str(error.exception))
            self.assertIn('Stage 2 word=00112244', str(error.exception))
            self.assertIn('Stage 1 nearby:', str(error.exception))
            self.assertIn('Stage 2 nearby:', str(error.exception))

    def test_third_generation_compiles_prepared_compiler_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(self.module, 'run', return_value=b'generated hex') as run:
                with mock.patch.object(self.module, 'assemble', return_value=b'compiled bytes'):
                    self.module.verify_fixed_point(
                        pathlib.Path('/example/nulang'), b'prepared compiler',
                        b'compiled bytes', pathlib.Path(tmp)
                    )
            self.assertEqual(run.call_args.args[1], b'prepared compiler')
            self.assertEqual(run.call_args.args[0][0], '/example/nulang')
            self.assertEqual(run.call_args.args[0][1], str(pathlib.Path(tmp) / 'self_compile.nbc'))

    def test_third_generation_rejects_different_bytecode(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(self.module, 'run', return_value=b'generated hex'):
                with mock.patch.object(self.module, 'assemble', return_value=b'different bytes'):
                    with self.assertRaisesRegex(AssertionError, 'fixed.point'):
                        self.module.verify_fixed_point(
                            pathlib.Path('/example/nulang'), b'prepared compiler',
                            b'compiled bytes', pathlib.Path(tmp)
                        )

    def test_third_generation_propagates_compiler_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(self.module, 'run', side_effect=RuntimeError('compiler exited 4')):
                with self.assertRaisesRegex(RuntimeError, 'exited 4'):
                    self.module.verify_fixed_point(
                        pathlib.Path('/example/nulang'), b'prepared compiler',
                        b'compiled bytes', pathlib.Path(tmp)
                    )


if __name__ == '__main__':
    unittest.main()
