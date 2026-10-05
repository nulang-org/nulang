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
    def test_measurement_and_build_plans_include_cold_jit_probe(self):
        extra_args = ["--no-default-features", "--features", "native-codegen"]

        measurement = nulang_ab_bench.cargo_commands(extra_args)
        builds = nulang_ab_bench.cargo_build_commands(extra_args)

        self.assertTrue(
            any("benchmarks::bench_" in command for command in measurement),
            "exact A/B must retain the established runtime benchmark suite",
        )
        self.assertTrue(
            any("--test" in command and "cold_jit_ab" in command for command in measurement),
            "exact A/B must execute the counterbalanced cold-JIT integration probe",
        )
        self.assertTrue(
            any("--test" in command and "cold_jit_ab" in command for command in builds),
            "cold-JIT integration probe must be prebuilt before warmup/measurement rounds",
        )


if __name__ == "__main__":
    unittest.main()
