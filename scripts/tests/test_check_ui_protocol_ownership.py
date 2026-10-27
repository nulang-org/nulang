"""Test renderer-neutral UI protocol source ownership."""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import check_ui_protocol_ownership as ownership


class UiOwnershipTests(unittest.TestCase):
    def test_protocol_imports_remain_in_web_boundary(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src' / 'web').mkdir(parents=True)
            (root / 'src' / 'runtime').mkdir()
            (root / 'src' / 'web' / 'reactivity.rs').write_text('use nulang_ui_protocol::UiDocument;')
            (root / 'src' / 'runtime' / 'mod.rs').write_text('pub fn run() {}')
            self.assertEqual(ownership.find_violations(root), [])

    def test_direct_runtime_dependency_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src' / 'runtime').mkdir(parents=True)
            (root / 'src' / 'runtime' / 'mod.rs').write_text('use nulang_ui_protocol::UiDocument;')
            self.assertEqual(ownership.find_violations(root), ['src/runtime/mod.rs:1'])

    def test_qualified_reference_is_rejected_even_without_use(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src').mkdir(parents=True)
            (root / 'src' / 'lib.rs').write_text('fn demo() { nulang_ui_protocol::decode_host_message(""); }')
            self.assertEqual(ownership.find_violations(root), ['src/lib.rs:1'])

    def test_missing_source_tree_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                ownership.find_violations(pathlib.Path(tmp))

    def test_comment_only_reference_does_not_trip_guard(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src').mkdir(parents=True)
            (root / 'src' / 'lib.rs').write_text('// nulang_ui_protocol is owned by web\nfn foo() {}\n')
            self.assertEqual(ownership.find_violations(root), [])

    def test_block_comment_does_not_trip_guard(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src').mkdir(parents=True)
            (root / 'src' / 'lib.rs').write_text('/* nulang_ui_protocol::Foo */\nfn foo() {}')
            self.assertEqual(ownership.find_violations(root), [])

    def test_guard_catches_multiline_use_and_lists_all_offenders(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src' / 'runtime').mkdir(parents=True)
            (root / 'src' / 'lib.rs').write_text('pub use\n  nulang_ui_protocol::{UiDocument};\n')
            (root / 'src' / 'runtime' / 'mod.rs').write_text('extern crate nulang_ui_protocol;')
            self.assertEqual(ownership.find_violations(root), ['src/lib.rs:2', 'src/runtime/mod.rs:1'])


if __name__ == '__main__':
    unittest.main()
