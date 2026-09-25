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


if __name__ == "__main__":
    unittest.main()
