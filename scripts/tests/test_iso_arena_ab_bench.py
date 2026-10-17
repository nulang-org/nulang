"""Contract tests for the paired active-iso-arena benchmark parser."""
import json
import math
import pathlib
import statistics
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import iso_arena_ab_bench as bench


class IsoArenaAbBenchTests(unittest.TestCase):
    def test_reads_criterion_median_ns_per_batch(self):
        with tempfile.TemporaryDirectory() as td:
            file = pathlib.Path(td) / "estimates.json"
            file.write_text(json.dumps({"median": {"point_estimate": 875.25}}))
            self.assertEqual(bench.read_median_ns(file), 875.25)

    def test_rejects_missing_or_invalid_criterion_estimate(self):
        with tempfile.TemporaryDirectory() as td:
            file = pathlib.Path(td) / "estimates.json"
            for payload in ({}, {"median": {"point_estimate": -5}},
                            {"median": {"point_estimate": "NaN"}}):
                file.write_text(json.dumps(payload))
                with self.assertRaises((ValueError, KeyError, TypeError)):
                    bench.read_median_ns(file)

    def test_paired_reduction_uses_matched_round_ratios(self):
        base = [100.0, 200.0, 300.0]
        candidate = [80.0, 160.0, 240.0]
        result = bench.paired_reduction(base, candidate)
        self.assertAlmostEqual(result["median_reduction_pct"], 20.0)
        self.assertEqual(result["pairs"], 3)
        self.assertEqual(len(result["paired_reductions_pct"]), 3)

    def test_paired_reduction_detects_invalid_samples(self):
        for base, candidate in [([1.0, 2.0], [1.0]), ([0.0], [2.0]),
                                ([1.0], [math.inf]), ([], [])]:
            with self.assertRaises(ValueError):
                bench.paired_reduction(base, candidate)

    def test_criterion_group_slash_is_sanitized_in_results_path(self):
        with tempfile.TemporaryDirectory() as td:
            home = pathlib.Path(td)
            expected = home / "actor_iso_arena" / "active_alloc_reset_128" / "new" / "estimates.json"
            self.assertEqual(
                bench.criterion_estimate_path(home, "active_alloc_reset_128"),
                expected,
            )

    def test_criterion_case_uses_isolated_explicit_criterion_home(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            target = root / "candidate"
            binary = root / "fake-benchmark"
            def fake_run(command, *, cwd, env):
                self.assertEqual(env["CRITERION_HOME"], str(target / "criterion"))
                estimate = target / "criterion" / "actor_iso_arena" / "idle_reset_128" / "new" / "estimates.json"
                estimate.parent.mkdir(parents=True, exist_ok=True)
                estimate.write_text(json.dumps({"median": {"point_estimate": 1280.5}}))
                return "Benchmarking actor/iso_arena/idle_reset_128"
            with patch.object(bench, "run", side_effect=fake_run):
                result = bench.criterion_case(binary, target, "idle_reset_128", None, 1.0, 2.0)
            self.assertEqual(result, 1280.5)

    def test_criterion_case_rejects_nonexistent_results_with_diagnostics(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            with patch.object(bench, "run", return_value="Benchmarking a different case"):
                with self.assertRaisesRegex(RuntimeError, "idle_reset_128"):
                    bench.criterion_case(root / "bench", root / "target", "idle_reset_128", None, 1.0, 2.0)

    def test_counterbalanced_run_order(self):
        self.assertEqual(bench.variant_order(0), ("base", "candidate"))
        self.assertEqual(bench.variant_order(1), ("candidate", "base"))
        self.assertEqual(bench.variant_order(2), ("base", "candidate"))


if __name__ == "__main__":
    unittest.main()
