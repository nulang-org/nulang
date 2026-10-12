"""Contract tests for exact-base cross-shard receive/drain comparison."""

import importlib.util
import unittest
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[1] / "cross_shard_drain_ab.py"
SPEC = importlib.util.spec_from_file_location("cross_shard_drain_ab", SOURCE)
assert SPEC is not None and SPEC.loader is not None
harness = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(harness)


class CrossShardDrainABTests(unittest.TestCase):
    def test_parser_requires_complete_measurement_record(self) -> None:
        output = "[cross-shard-drain] batch=4 rounds=100 elapsed_ns=12345"
        self.assertEqual(harness.parse_measurement(output, 4, 100), 12345)
        with self.assertRaisesRegex(RuntimeError, "missing"):
            harness.parse_measurement("nothing was measured", 4, 100)

    def test_parser_rejects_mismatched_workload(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "mismatch"):
            harness.parse_measurement(
                "[cross-shard-drain] batch=8 rounds=100 elapsed_ns=12345", 4, 100
            )

    def test_pair_summary_reports_slower_candidate(self) -> None:
        summary = harness.summarize([1000, 1000, 1000, 1000], [1100] * 4)
        self.assertEqual(summary["pairs"], 4)
        self.assertAlmostEqual(summary["median_latency_change_pct"], 10.0)
        self.assertLess(summary["speedup_ci95_upper"], 1.0)

    def test_pair_summary_rejects_missing_pairs(self) -> None:
        with self.assertRaisesRegex(ValueError, "paired"):
            harness.summarize([100.0], [100.0, 200.0])

    def test_workloads_cover_small_and_bursty_drains(self) -> None:
        self.assertEqual(harness.BATCHES, (1, 4, 16))


if __name__ == "__main__":
    unittest.main()
