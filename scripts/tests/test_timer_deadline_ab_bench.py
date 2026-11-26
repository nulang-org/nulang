"""Contract tests for the isolated timer deadline comparison harness."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "timer_deadline_ab_bench.py"
spec = importlib.util.spec_from_file_location("timer_deadline_ab_bench", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class TimerBenchContractTests(unittest.TestCase):
    def test_overlay_accepts_only_exact_benchmark_registration(self):
        base = "mod scheduler_bench;\nmod vm_bench;\n" + "    scheduler_bench::benches,\n" * 2
        candidate = module.expected_bench_entry(base)
        self.assertEqual(module.matched_bench_entry(base, candidate), candidate)

    def test_overlay_rejects_any_unrelated_benchmark_edit(self):
        base = "mod scheduler_bench;\nmod vm_bench;\n" + "    scheduler_bench::benches,\n" * 2
        candidate = module.expected_bench_entry(base)
        with self.assertRaisesRegex(RuntimeError, "unrelated"):
            module.matched_bench_entry(base, candidate.replace("mod vm_bench;", "mod modified_bench;"))

    def test_bench_entry_missing_anchor_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            module.expected_bench_entry("mod unrelated;\n")

    def test_estimates_require_all_six_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for lane in module.LANES:
                for count in module.COUNTS:
                    path = root / "criterion" / "timer_next_deadline" / lane / str(count) / "new" / "estimates.json"
                    path.parent.mkdir(parents=True)
                    path.write_text(json.dumps({"mean": {"point_estimate": 42.0}}))
            result = module.read_estimates(root)
            self.assertEqual(len(result), 6)
            (root / "criterion/timer_next_deadline/active_root/1024/new/estimates.json").unlink()
            with self.assertRaisesRegex(RuntimeError, "missing"):
                module.read_estimates(root)

    def test_pairing_is_round_aligned(self):
        result = module.summarize_pairs([100, 200, 400], [80, 250, 200])
        self.assertEqual(result["pairs"], 3)
        self.assertAlmostEqual(result["median_speedup_x"], 1.25)

    def test_invalid_or_partial_pairs_fail_closed(self):
        with self.assertRaisesRegex(RuntimeError, "unequal"):
            module.summarize_pairs([100, 200], [100])
        with self.assertRaisesRegex(RuntimeError, "invalid"):
            module.summarize_pairs([1, float("nan")], [1, 2])
        with self.assertRaisesRegex(RuntimeError, "invalid"):
            module.summarize_pairs([0, 2], [1, 2])


if __name__ == "__main__":
    unittest.main()
