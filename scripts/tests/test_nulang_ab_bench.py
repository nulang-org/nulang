import contextlib
import importlib.util
import io
import pathlib
import subprocess
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "nulang_ab_bench.py"

spec = importlib.util.spec_from_file_location("nulang_ab_bench", SCRIPT)
nulang_ab_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nulang_ab_bench)


class RecordParserTests(unittest.TestCase):
    EXPECTED = {
        "counting": 200_000,
        "ping_pong": 40_001,
        "thread_ring": 20_000,
        "fork_join": 100_000,
    }

    def valid_output(self):
        return "\n".join(
            [
                f"[cross-bench] runtime=nulang benchmark={name} "
                f"messages={count} elapsed_ns=1000"
                for name, count in self.EXPECTED.items()
            ]
            + [
                "[cross-bench] runtime=nulang benchmark=skynet "
                "messages=1110 elapsed_ns=1200",
                "[ab-bench] benchmark=enqueue_payload_1 "
                "operations=100000 elapsed_ns=2200",
            ]
        )

    def test_valid_cross_and_extra_ab_records_are_preserved(self):
        rows = nulang_ab_bench.parse_records(self.valid_output())
        self.assertEqual(set(self.EXPECTED) | {"skynet", "ab/enqueue_payload_1"}, set(rows))
        self.assertEqual(100_000, rows["ab/enqueue_payload_1"]["messages"])

    def test_duplicate_cross_record_is_rejected(self):
        output = self.valid_output() + "\n" + (
            "[cross-bench] runtime=nulang benchmark=counting "
            "messages=200000 elapsed_ns=800"
        )
        with self.assertRaisesRegex(RuntimeError, "duplicate.*counting"):
            nulang_ab_bench.parse_records(output)

    def test_duplicate_ab_record_is_rejected(self):
        output = self.valid_output() + "\n" + (
            "[ab-bench] benchmark=enqueue_payload_1 "
            "operations=100000 elapsed_ns=900"
        )
        with self.assertRaisesRegex(RuntimeError, "duplicate.*enqueue_payload_1"):
            nulang_ab_bench.parse_records(output)

    def test_changed_known_cross_workload_count_is_rejected(self):
        output = self.valid_output().replace(
            "benchmark=counting messages=200000",
            "benchmark=counting messages=199999",
        )
        with self.assertRaisesRegex(RuntimeError, "counting.*expected 200000"):
            nulang_ab_bench.parse_records(output)

    def test_zero_elapsed_time_is_rejected_at_parse_time(self):
        output = self.valid_output().replace(
            "operations=100000 elapsed_ns=2200",
            "operations=100000 elapsed_ns=0",
        )
        with self.assertRaisesRegex(RuntimeError, "elapsed_ns must be positive"):
            nulang_ab_bench.parse_records(output)

    def test_wrong_runtime_record_is_rejected(self):
        output = self.valid_output() + "\n" + (
            "[cross-bench] runtime=go benchmark=counting "
            "messages=200000 elapsed_ns=2000"
        )
        with self.assertRaisesRegex(RuntimeError, "unexpected runtime"):
            nulang_ab_bench.parse_records(output)

    def test_malformed_ab_record_is_rejected_instead_of_ignored(self):
        output = self.valid_output() + "\n" + (
            "[ab-bench] benchmark=broken operations=100 elapsed_ns=NaN"
        )
        with self.assertRaisesRegex(RuntimeError, "malformed.*ab-bench"):
            nulang_ab_bench.parse_records(output)

    def test_missing_mandatory_cross_workload_is_rejected(self):
        output = "\n".join(
            line for line in self.valid_output().splitlines()
            if "benchmark=thread_ring" not in line
        )
        with self.assertRaisesRegex(RuntimeError, "thread_ring"):
            nulang_ab_bench.parse_records(output)


class PairedComparisonTests(unittest.TestCase):
    def test_paired_comparisons_use_same_round_ratios(self):
        samples = {
            "base": {
                "work": [
                    {"messages": 100, "elapsed_ns": 100},
                    {"messages": 100, "elapsed_ns": 200},
                    {"messages": 100, "elapsed_ns": 400},
                ]
            },
            "candidate": {
                "work": [
                    {"messages": 100, "elapsed_ns": 80},
                    {"messages": 100, "elapsed_ns": 250},
                    {"messages": 100, "elapsed_ns": 200},
                ]
            },
        }

        paired = nulang_ab_bench.paired_comparisons(samples)
        row = paired["work"]

        self.assertEqual(3, row["pairs"])
        self.assertAlmostEqual(1.25, row["median_speedup_x"])
        self.assertAlmostEqual(25.0, row["median_throughput_change_pct"])
        self.assertAlmostEqual(-20.0, row["median_latency_change_pct"])
        self.assertLessEqual(row["speedup_ci95_lower"], row["median_speedup_x"])
        self.assertGreaterEqual(row["speedup_ci95_upper"], row["median_speedup_x"])

    def test_paired_comparisons_reject_misaligned_sample_counts(self):
        samples = {
            "base": {
                "work": [
                    {"messages": 100, "elapsed_ns": 100},
                    {"messages": 100, "elapsed_ns": 100},
                ]
            },
            "candidate": {
                "work": [
                    {"messages": 100, "elapsed_ns": 100},
                ]
            },
        }

        with self.assertRaisesRegex(RuntimeError, "paired sample count changed"):
            nulang_ab_bench.paired_comparisons(samples)

    def test_paired_comparisons_reject_changed_operation_count(self):
        samples = {
            "base": {"work": [{"messages": 100, "elapsed_ns": 100}]},
            "candidate": {"work": [{"messages": 99, "elapsed_ns": 100}]},
        }

        with self.assertRaisesRegex(RuntimeError, "operation count changed"):
            nulang_ab_bench.paired_comparisons(samples)


class CommandOutputTests(unittest.TestCase):
    def test_failed_command_preserves_captured_diagnostics_on_stderr(self):
        completed = subprocess.CompletedProcess(
            args=["cargo", "test"],
            returncode=101,
            stdout="error: candidate compiler diagnostic\n",
        )
        stderr = io.StringIO()

        with mock.patch.object(nulang_ab_bench.subprocess, "run", return_value=completed):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    nulang_ab_bench.command_output(
                        ["cargo", "test"],
                        cwd=ROOT,
                    )

        self.assertIn("candidate compiler diagnostic", stderr.getvalue())
        self.assertEqual(
            "error: candidate compiler diagnostic\n",
            raised.exception.output,
        )


class CargoIsolationTests(unittest.TestCase):
    def test_base_and_candidate_use_distinct_cargo_target_dirs(self):
        base = nulang_ab_bench.cargo_environment("base")
        candidate = nulang_ab_bench.cargo_environment("candidate")

        self.assertIn("CARGO_TARGET_DIR", base)
        self.assertIn("CARGO_TARGET_DIR", candidate)
        self.assertNotEqual(base["CARGO_TARGET_DIR"], candidate["CARGO_TARGET_DIR"])

    def test_variant_cargo_environment_preserves_process_environment(self):
        with mock.patch.dict(nulang_ab_bench.os.environ, {"NULANG_AB_SENTINEL": "present"}):
            env = nulang_ab_bench.cargo_environment("candidate")

        self.assertEqual("present", env["NULANG_AB_SENTINEL"])


class CargoCommandPlanTests(unittest.TestCase):
    def test_measurement_filter_includes_integration_benchmark_probes(self):
        extra_args = ["--no-default-features", "--features", "native-codegen"]

        measurement = nulang_ab_bench.cargo_command(extra_args)
        build = nulang_ab_bench.cargo_build_command(extra_args)

        self.assertIn("bench_", measurement)
        self.assertIn("bench_", build)
        self.assertNotIn(
            "benchmarks::bench_",
            measurement,
            "module-qualified filter excludes tests/cold_jit_ab.rs from exact A/B",
        )
        self.assertNotIn(
            "benchmarks::bench_",
            build,
            "prebuild must cover integration benchmark executables too",
        )


if __name__ == "__main__":
    unittest.main()
