"""Contract tests for the compact ORCA paired benchmark reporter."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench_orca_header_profiles import criterion_means, summarize  # noqa: E402


class OrcaHeaderBenchmarkReporterTests(unittest.TestCase):
    def test_reads_criterion_mean_from_group_parameter(self):
        with tempfile.TemporaryDirectory() as folder:
            criterion = Path(folder)
            file = criterion / "gc" / "actor_heap_small_alloc" / "payload_16" / "new" / "estimates.json"
            file.parent.mkdir(parents=True)
            file.write_text(json.dumps({"mean": {"point_estimate": 123.5}}), encoding="utf-8")
            second = criterion / "gc" / "orca_local_alloc_drop" / "payload_16" / "new" / "estimates.json"
            second.parent.mkdir(parents=True)
            second.write_text(json.dumps({"mean": {"point_estimate": 200.0}}), encoding="utf-8")
            self.assertEqual(
                criterion_means(criterion),
                {
                    "gc/actor_heap_small_alloc/payload_16": 123.5,
                    "gc/orca_local_alloc_drop/payload_16": 200.0,
                },
            )

    def test_fails_if_one_benchmark_group_did_not_run(self):
        with tempfile.TemporaryDirectory() as folder:
            criterion = Path(folder)
            file = criterion / "gc" / "actor_heap_small_alloc" / "payload_16" / "new" / "estimates.json"
            file.parent.mkdir(parents=True)
            file.write_text(json.dumps({"mean": {"point_estimate": 123.5}}), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "Missing Criterion groups"):
                criterion_means(criterion)

    def test_summarizes_balanced_abba_without_inventing_significance(self):
        samples = [
            {"profile": "default", "means_ns": {"gc/actor_heap_small_alloc/payload_16": 100}},
            {"profile": "compact", "means_ns": {"gc/actor_heap_small_alloc/payload_16": 80}},
            {"profile": "compact", "means_ns": {"gc/actor_heap_small_alloc/payload_16": 90}},
            {"profile": "default", "means_ns": {"gc/actor_heap_small_alloc/payload_16": 110}},
        ]
        row = summarize(samples)[0]
        self.assertAlmostEqual(row["default_mean_ns"], 105)
        self.assertAlmostEqual(row["compact_mean_ns"], 85)
        self.assertAlmostEqual(row["latency_change_pct"], -19.0476190476)
        self.assertEqual(row["default_samples"], 2)
        self.assertEqual(row["compact_samples"], 2)

    def test_rejects_missing_comparison_profile(self):
        with self.assertRaisesRegex(ValueError, "Missing one profile"):
            summarize(
                [{"profile": "compact", "means_ns": {"gc/orca_local_alloc_drop/payload_8": 1.0}}]
            )


if __name__ == "__main__":
    unittest.main()
