"""Contract tests for paired empty-selective-receive Criterion measurement."""
import json
import math
import pathlib
import statistics
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import selective_receive_ab_bench as bench


class SelectiveReceiveAbBenchTests(unittest.TestCase):
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
            expected = home / "actor_selective_receive_empty" / "repeated_poll" / "new" / "estimates.json"
            self.assertEqual(
                bench.criterion_estimate_path(home, "repeated_poll"),
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
                estimate = target / "criterion" / "actor_selective_receive_empty" / "first_poll" / "new" / "estimates.json"
                estimate.parent.mkdir(parents=True, exist_ok=True)
                estimate.write_text(json.dumps({"median": {"point_estimate": 1280.5}}))
                return "Benchmarking actor/selective_receive_empty/first_poll"
            with patch.object(bench, "run", side_effect=fake_run):
                result = bench.criterion_case(binary, target, "first_poll", None, 1.0, 2.0)
            self.assertEqual(result, 1280.5)

    def test_single_cpu_mode_fails_closed_when_taskset_is_missing(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            with patch.object(bench.shutil, "which", return_value=None):
                with patch.object(bench, "run") as subprocess:
                    with self.assertRaisesRegex(RuntimeError, "taskset"):
                        bench.criterion_case(
                            root / "bench", root / "target", "first_poll",
                            0, 1.0, 2.0,
                        )
                    subprocess.assert_not_called()

    def test_single_cpu_mode_uses_taskset_for_each_criterion_sample(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            target = root / "candidate"
            def fake_run(command, *, cwd, env):
                self.assertEqual(command[:3], ["taskset", "-c", "3"])
                estimate = (target / "criterion" / "actor_selective_receive_empty"
                            / "first_poll" / "new" / "estimates.json")
                estimate.parent.mkdir(parents=True, exist_ok=True)
                estimate.write_text(json.dumps({"median": {"point_estimate": 400.0}}))
                return "Benchmarking actor/selective_receive_empty/first_poll"
            with patch.object(bench.shutil, "which", return_value="/usr/bin/taskset"):
                with patch.object(bench, "run", side_effect=fake_run):
                    self.assertEqual(
                        bench.criterion_case(
                            root / "bench", target, "first_poll", 3, 1.0, 2.0,
                        ), 400.0,
                    )

    def test_criterion_case_rejects_nonexistent_results_with_diagnostics(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            with patch.object(bench, "run", return_value="Benchmarking a different case"):
                with self.assertRaisesRegex(RuntimeError, "first_poll"):
                    bench.criterion_case(root / "bench", root / "target", "first_poll", None, 1.0, 2.0)

    def test_first_message_after_empty_poll_is_required(self):
        self.assertEqual(
            bench.CASES,
            ("first_poll", "repeated_poll", "empty_then_arrival"),
            "A win on empty-only polls may merely defer index allocation "
            "until the first real message; the comparator must measure both",
        )
        with tempfile.TemporaryDirectory() as td:
            path = pathlib.Path(td)
            self.assertEqual(
                bench.criterion_estimate_path(path, "empty_then_arrival"),
                path / "actor_selective_receive_empty"
                / "empty_then_arrival" / "new" / "estimates.json",
            )

    def test_counterbalanced_run_order(self):
        self.assertEqual(bench.variant_order(0), ("base", "candidate"))
        self.assertEqual(bench.variant_order(1), ("candidate", "base"))
        self.assertEqual(bench.variant_order(2), ("base", "candidate"))


if __name__ == "__main__":
    unittest.main()
