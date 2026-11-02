"""Contract tests for the deliberately strict source-layout oracle."""
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'bootstrap' / 'verify_layout_invariance.py'


class LayoutInvarianceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('layout_invariance', SCRIPT)
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_removes_only_one_unused_helper(self):
        source = 'head\nfn instr(x: Int) -> Int {\n    x\n}\n\nfn emit_word(x: Int) -> Int { x }\ntail\n'
        self.assertEqual(
            self.module.without_unused_instr(source),
            'head\nfn emit_word(x: Int) -> Int { x }\ntail\n',
        )

    def test_refuses_missing_helper(self):
        with self.assertRaisesRegex(ValueError, 'exactly one'):
            self.module.without_unused_instr('fn emit_word(x: Int) -> Int { x }')

    def test_refuses_removal_when_helper_is_used(self):
        source = 'fn instr(x: Int) -> Int { x }\n\nfn emit_word(x: Int) -> Int { instr(x) }'
        with self.assertRaisesRegex(ValueError, 'still referenced'):
            self.module.without_unused_instr(source)

    def test_refuses_malformed_helper_boundary(self):
        with self.assertRaisesRegex(ValueError, 'end before'):
            self.module.without_unused_instr('fn emit_word(x: Int) -> Int { x }\nfn instr(x: Int) -> Int { x }')


if __name__ == '__main__':
    unittest.main()
