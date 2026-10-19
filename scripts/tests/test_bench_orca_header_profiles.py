"""Contract tests for the compact ORCA paired benchmark reporter."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench_orca_header_profiles import assert_checkout_matches, criterion_means, summarize  # noqa: E402


class OrcaHeaderBenchmarkReporterTests(unittest.TestCase):
    def test_reads_all_criterion_means_from_group_and_parameter(self):
        with tempfile.TemporaryDirectory() as folder:
            criterion = Path(folder)
            expected = {}
            for group, estimate in (
                ("actor_heap_small_alloc", 123.5),
                ("orca_local_alloc_drop", 200.0),
            ):
                for payload_size in (8, 16, 32, 64, 128):
                    file = (
                        criterion / "gc" / group / f"payload_{payload_size}"
                        / "new" / "estimates.json"
                    )
                    file.parent.mkdir(parents=True, exist_ok=True)
                    file.write_text(
                        json.dumps({"mean": {"point_estimate": estimate}}),
                        encoding="utf-8",
                    )
                    expected[f"gc/{group}/payload_{payload_size}"] = estimate
            self.assertEqual(criterion_means(criterion), expected)

    def test_fails_if_one_benchmark_group_did_not_run(self):
        with tempfile.TemporaryDirectory() as folder:
            criterion = Path(folder)
            file = criterion / "gc" / "actor_heap_small_alloc" / "payload_16" / "new" / "estimates.json"
            file.parent.mkdir(parents=True)
            file.write_text(json.dumps({"mean": {"point_estimate": 123.5}}), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "Missing Criterion groups"):
                criterion_means(criterion)

    def test_rejects_missing_payload_variant_even_when_both_groups_exist(self):
        with tempfile.TemporaryDirectory() as folder:
            criterion = Path(folder)
            for group in ("actor_heap_small_alloc", "orca_local_alloc_drop"):
                file = criterion / "gc" / group / "payload_16" / "new" / "estimates.json"
                file.parent.mkdir(parents=True)
                file.write_text(
                    json.dumps({"mean": {"point_estimate": 123.5}}),
                    encoding="utf-8",
                )
            with self.assertRaisesRegex(RuntimeError, "Missing Criterion benchmarks"):
                criterion_means(criterion)

    def test_rejects_missing_profile_workload_in_summary(self):
        samples = [
            {"profile": "default", "means_ns": {
                "gc/actor_heap_small_alloc/payload_16": 100,
                "gc/orca_local_alloc_drop/payload_16": 150,
            }},
            {"profile": "compact", "means_ns": {
                "gc/actor_heap_small_alloc/payload_16": 90,
            }},
        ]
        with self.assertRaisesRegex(ValueError, "Mismatched benchmark IDs"):
            summarize(samples)

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

    def test_checkout_guard_rejects_head_changes_between_trials(self):
        with patch("bench_orca_header_profiles.git", return_value="different-head"):
            with self.assertRaisesRegex(RuntimeError, "HEAD moved"):
                assert_checkout_matches("original-head", allow_dirty=False)

    def test_checkout_guard_rejects_dirty_worktree(self):
        with patch("bench_orca_header_profiles.git", side_effect=["expected", " M src/runtime/heap.rs"]):
            with self.assertRaisesRegex(RuntimeError, "dirty worktree"):
                assert_checkout_matches("expected", allow_dirty=False)

    def test_checkout_guard_allows_explicitly_recorded_dirty_override(self):
        with patch("bench_orca_header_profiles.git", return_value="expected") as spy:
            assert_checkout_matches("expected", allow_dirty=True)
            spy.assert_called_once_with("rev-parse", "HEAD")

    def test_rejects_missing_comparison_profile(self):
        with self.assertRaisesRegex(ValueError, "Missing one profile"):
            summarize(
                [{"profile": "compact", "means_ns": {"gc/orca_local_alloc_drop/payload_8": 1.0}}]
            )


if __name__ == "__main__":
    unittest.main()
