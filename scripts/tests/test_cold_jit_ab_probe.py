import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
PROBE = ROOT / "tests" / "cold_jit_ab.rs"


class ColdJitAbProbeContractTests(unittest.TestCase):
    def test_exact_ab_probe_tracks_jit_off_and_on_without_tiering(self):
        source = PROBE.read_text()
        self.assertIn('report_ab("interp_cold_jit_off"', source)
        self.assertIn('report_ab("interp_cold_jit_on"', source)
        self.assertIn("VM::new_without_jit()", source)
        self.assertIn("VM::new()", source)
        self.assertIn("jit_compiled_count()", source)

    def test_probe_counterbalances_each_repetition(self):
        source = PROBE.read_text()
        self.assertIn("for repetition in 0..REPEATS", source)
        self.assertIn("repetition % 2 == 0", source)
        self.assertIn("timed_run(&mut interp_vm", source)
        self.assertIn("timed_run(&mut jit_vm", source)
        self.assertNotIn("Vec<VM>", source)


if __name__ == "__main__":
    unittest.main()
