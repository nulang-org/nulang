import contextlib
import importlib.util
import io
import json
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "confirm_bench_regression.py"

spec = importlib.util.spec_from_file_location("confirm_bench_regression", SCRIPT)
confirm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(confirm)


class CriterionFilterTests(unittest.TestCase):
    def test_filter_is_anchored_and_regex_escaped(self):
        value = confirm.criterion_filter(["group/a+b", "scheduler/foo/1000"])
        self.assertEqual(r"^(?:group/a\+b|scheduler/foo/1000)$", value)


    def test_filter_preserves_exact_criterion_id(self):
        pattern = confirm.criterion_filter(["persist/checkpoint_json_encode/1kb"])
        self.assertRegex("persist/checkpoint_json_encode/1kb", pattern)
        self.assertNotRegex("persist_checkpoint_json_encode/1kb", pattern)

    def test_report_metadata_round_trip_preserves_group_underscores(self):
        with tempfile.TemporaryDirectory() as tmp:
            criterion_dir = pathlib.Path(tmp) / "criterion"
            metadata_path = (
                criterion_dir
                / "actor_selective_receive_depth"
                / "64"
                / "new"
                / "benchmark.json"
            )
            metadata_path.parent.mkdir(parents=True)
            metadata_path.write_text(
                json.dumps(
                    {
                        "directory_name": "actor_selective_receive_depth/64",
                        "full_id": "actor/selective_receive_depth/64",
                    }
                )
            )

            report_benchmark_ids = confirm.benchmark_ids_for_reports(
                criterion_dir, ["actor_selective_receive_depth/64"]
            )

        self.assertEqual(
            {
                "actor_selective_receive_depth/64": (
                    "actor/selective_receive_depth/64"
                )
            },
            report_benchmark_ids,
        )
        pattern = confirm.criterion_filter(list(report_benchmark_ids.values()))
        self.assertRegex("actor/selective_receive_depth/64", pattern)
        self.assertNotRegex("actor/selective/receive/depth/64", pattern)

    def test_jit_interpreter_history_key_maps_to_real_criterion_full_id(self):
        # Incident 39152376887: a sanitized report key selected zero benchmarks.
        with tempfile.TemporaryDirectory() as tmp:
            criterion_dir = pathlib.Path(tmp) / "criterion"
            metadata_path = (
                criterion_dir
                / "jit_function_call_loop_interp"
                / "new"
                / "benchmark.json"
            )
            metadata_path.parent.mkdir(parents=True)
            metadata_path.write_text(
                json.dumps(
                    {
                        "directory_name": "jit_function_call_loop_interp",
                        "full_id": "jit/function_call_loop_interp",
                    }
                )
            )
            mapped = confirm.benchmark_ids_for_reports(
                criterion_dir, ["jit_function_call_loop_interp"]
            )
            pattern = confirm.criterion_filter(list(mapped.values()))
            self.assertEqual(
                "jit/function_call_loop_interp", mapped["jit_function_call_loop_interp"]
            )
            self.assertRegex("jit/function_call_loop_interp", pattern)
            self.assertNotRegex("jit_function_call_loop_interp", pattern)

    def test_jit_filtered_result_remains_keyed_by_historical_report_path(self):
        report_key = "jit_function_call_loop_interp"
        runtime_id = "jit/function_call_loop_interp"
        with tempfile.TemporaryDirectory() as tmp:
            target_dir = pathlib.Path(tmp) / "target"
            estimate = (
                target_dir / "criterion" / report_key / "new" / "estimates.json"
            )

            def fake_benchmark_run(command, **kwargs):
                self.assertIn(confirm.criterion_filter([runtime_id]), command)
                estimate.parent.mkdir(parents=True)
                estimate.write_text(json.dumps({"mean": {"point_estimate": 42.0}}))
                return ""

            with mock.patch.object(
                confirm, "command_output", side_effect=fake_benchmark_run
            ):
                measured = confirm.run_filtered_benchmarks(
                    root=pathlib.Path(tmp),
                    env={},
                    target_dir=target_dir,
                    report_benchmark_ids={report_key: runtime_id},
                    affinity=None,
                )
        self.assertEqual({report_key: 42.0}, measured)

    def test_metadata_lookup_rejects_unmapped_report_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(RuntimeError, "did not map historical"):
                confirm.benchmark_ids_for_reports(
                    pathlib.Path(tmp), ["persist_checkpoint_json_encode/1kb"]
                )

class ManifestValidationTests(unittest.TestCase):
    def test_rejects_non_object_regression_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "manifest.json"
            path.write_text(json.dumps({"schema": 1, "regressions": ["not-an-object"]}))
            with self.assertRaisesRegex(RuntimeError, "non-object regression row"):
                confirm.load_regression_manifest(path)

    def test_rejects_non_finite_thresholds(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "manifest.json"
            path.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "regressions": [{"benchmark": "scheduler/foo", "threshold": "nan"}],
                    }
                )
            )
            with self.assertRaisesRegex(RuntimeError, "threshold must be finite"):
                confirm.load_regression_manifest(path)


class RoundSchedulingTests(unittest.TestCase):
    def test_four_rounds_are_order_balanced(self):
        orders = [confirm.measurement_order(i) for i in range(4)]
        self.assertEqual(
            [
                ("base", "candidate"),
                ("candidate", "base"),
                ("base", "candidate"),
                ("candidate", "base"),
            ],
            orders,
        )

    def test_odd_run_counts_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "even --runs value"):
            confirm.validate_run_count(3)
        confirm.validate_run_count(4)


class ReportContractTests(unittest.TestCase):
    def test_empty_manifest_emits_full_non_required_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            manifest = root / "manifest.json"
            output = root / "report.json"
            manifest.write_text(json.dumps({"schema": 1, "regressions": []}))
            argv = [
                "confirm_bench_regression.py",
                "--base-ref",
                "HEAD^",
                "--manifest",
                str(manifest),
                "--runs",
                "4",
                "--output",
                str(output),
            ]
            with mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(0, confirm.main())
            report = json.loads(output.read_text())

        self.assertEqual("not_required", report["status"])
        self.assertEqual(0, report["measured_runs"])
        self.assertEqual({"base": {}, "candidate": {}}, report["samples"])
        self.assertEqual([], report["analysis"])
        self.assertEqual([], report["confirmed"])

    def test_operational_errors_have_distinct_exit_code(self):
        stderr = io.StringIO()
        with mock.patch.object(confirm, "main", side_effect=RuntimeError("boom")):
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(2, confirm.entrypoint())
        self.assertIn("boom", stderr.getvalue())


class PairedRegressionTests(unittest.TestCase):
    def test_confirms_consistent_regression_beyond_historical_threshold(self):
        result = confirm.analyze_pairs(
            benchmark="scheduler/foo/1000",
            threshold=0.20,
            base_ns=[100.0, 102.0, 98.0, 101.0, 99.0],
            candidate_ns=[140.0, 141.0, 139.0, 142.0, 138.0],
        )

        self.assertTrue(result["confirmed"])
        self.assertGreater(result["median_latency_ratio"], 1.20)
        self.assertGreater(result["latency_ratio_ci95_lower"], 1.0)

    def test_rejects_shared_host_slowdown_that_affects_both_variants(self):
        result = confirm.analyze_pairs(
            benchmark="scheduler/foo/1000",
            threshold=0.20,
            base_ns=[200.0, 205.0, 198.0, 210.0, 202.0],
            candidate_ns=[202.0, 203.0, 201.0, 208.0, 204.0],
        )

        self.assertFalse(result["confirmed"])
        self.assertLess(result["median_latency_ratio"], 1.20)

    def test_requires_consistent_direction_not_just_large_median(self):
        result = confirm.analyze_pairs(
            benchmark="scheduler/foo/1000",
            threshold=0.20,
            base_ns=[100.0, 100.0, 100.0, 100.0, 100.0],
            candidate_ns=[80.0, 80.0, 140.0, 140.0, 140.0],
        )

        self.assertFalse(result["confirmed"])
        self.assertGreater(result["median_latency_ratio"], 1.20)
        self.assertLessEqual(result["latency_ratio_ci95_lower"], 1.0)


if __name__ == "__main__":
    unittest.main()
