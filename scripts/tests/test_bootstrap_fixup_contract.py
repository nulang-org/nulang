"""Pin the Python fixup algorithm before attempting a Nulang-native replacement.

These independent expected instruction words are protocol regression fixtures,
not snapshots regenerated from fixup_hex.py.
"""
import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "bootstrap" / "fixup_hex.py"


def load_fixup():
    spec = importlib.util.spec_from_file_location("bootstrap_fixup", SOURCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fixed_words(result):
    return [line for line in result if len(line) == 8
            and all(c in "0123456789abcdef" for c in line.lower())]


class BootstrapFixupContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixup = load_fixup().fixup

    def test_nested_if_offsets_target_nearest_own_boundaries(self):
        source = [
            "; if", "; JmpF -> else", "52000000", "04010000",
            "; Jmp -> end", "50000000", "; else:", "04020000",
            "; end:", "12010000",
        ]
        self.assertEqual(
            fixed_words(self.fixup(source)),
            ["52000003", "04010000", "50000200", "04020000", "12010000"],
        )

    def test_and_short_circuit_patches_both_targets(self):
        source = [
            "; and", "; JmpF -> and_fill", "52010000", "04000000",
            "; Jmp -> and_cont", "50000000", "; and_fill:",
            "04000000", "; and_cont:", "12010000",
        ]
        self.assertEqual(
            fixed_words(self.fixup(source)),
            ["52010003", "04000000", "50000200", "04000000", "12010000"],
        )

    def test_or_short_circuit_targets_right_fill_and_continuation(self):
        source = [
            "; or", "; JmpF -> or_right", "52010000",
            "; Jmp -> or_fill", "50000000", "; or_right:",
            "04000000", "; Jmp -> or_cont", "50000000",
            "; or_fill:", "03000000", "; or_cont:", "12010000",
        ]
        self.assertEqual(
            fixed_words(self.fixup(source)),
            ["52010002", "50000300", "04000000", "50000200",
             "03000000", "12010000"],
        )

    def test_nested_function_skips_match_own_fn_end_and_indices(self):
        source = [
            "; Jmp -> fn_end", "50000000", "; FN_START outer",
            "; Jmp -> fn_end", "50000000", "; FN_START inner",
            "04000000", "; fn_end:", "60000001", "12010000",
            "; fn_end:", "60000002", "12020000",
        ]
        self.assertEqual(
            fixed_words(self.fixup(source)),
            ["50000500", "50000200", "04000000", "60000201",
             "12010000", "60000102", "12020000"],
        )

    def test_integer_and_string_constants_keep_distinct_stable_indices(self):
        source = [
            "; preface", "; const 5", "07000001",
            "; const 5", "07000002",
            '; const "x"', "90000003",
        ]
        result = self.fixup(source)
        self.assertIn('; constant pool: [5, "x"]', result)
        self.assertEqual(
            fixed_words(result),
            ["07000001", "07000002", "90000103"],
        )

    def test_host_hex_placeholder_is_patched_without_dropping_instruction(self):
        source = ["; debug marker", "1200()000"]
        self.assertEqual(fixed_words(self.fixup(source)), ["1200a000"])

    def test_two_complement_branch_encoding_contract(self):
        module = load_fixup()
        self.assertEqual(module.format_hex(module.patch_jmp(0x50000000, -1)), "50ffff00")
        self.assertEqual(module.format_hex(module.patch_jmpf(0x52020000, -1)), "5202ffff")


if __name__ == "__main__":
    unittest.main()
