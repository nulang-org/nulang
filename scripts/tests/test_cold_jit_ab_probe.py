import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
PROBE = ROOT / "tests" / "cold_jit_ab.rs"


class ColdJitAbProbeContractTests(unittest.TestCase):
    def test_exact_ab_probe_counterbalances_jit_off_and_on_per_repetition(self):
        source = PROBE.read_text()
        self.assertIn('report_ab("interp_cold_jit_off"', source)
        self.assertIn('report_ab("interp_cold_jit_on"', source)
        self.assertIn("VM::new_without_jit()", source)
        self.assertIn("VM::new()", source)
        self.assertIn("jit_compiled_count()", source)
        self.assertIn("for repetition in 0..REPEATS", source)
        self.assertIn("if repetition % 2 == 0", source)
        self.assertNotIn("Vec<VM>", source)


if __name__ == "__main__":
    unittest.main()
