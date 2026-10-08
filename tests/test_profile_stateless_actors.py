"""Offline contract tests for the opt-in Linux perf profiling harness."""
import json
import unittest
from pathlib import Path

from scripts import profile_stateless_actors as profile


def row(benchmark="ping_pong", iteration=1, elapsed_ns=123456):
    return json.dumps({
        "schema": 1, "runtime": "nulang", "suite": "savina-style",
        "benchmark": benchmark, "iteration": iteration,
        "messages": 40001, "elapsed_ns": elapsed_ns,
    })


class ProfileContractTests(unittest.TestCase):
    def test_phase_records_require_verified_split_and_runtime(self):
        records = [
            {
                "runtime": "nulang", "suite": "savina-style", "schema": 1,
                "benchmark": "counting", "iteration": 1, "messages": 200000,
                "elapsed_ns": 80000, "phase_admission_ns": 25000,
                "phase_scheduler_ns": 55000,
            },
            {
                "runtime": "nulang", "suite": "savina-style", "schema": 1,
                "benchmark": "counting", "iteration": 2, "messages": 200000,
                "elapsed_ns": 75000, "phase_admission_ns": 23000,
                "phase_scheduler_ns": 52000,
            },
        ]
        summary = profile.validate_phase_samples(
            "\\n".join(json.dumps(row) for row in records),
            "counting", 2, 200000,
        )
        self.assertEqual(summary["samples"], 2)
        self.assertEqual(summary["median_admission_ns"], 24000)
        self.assertEqual(summary["median_scheduler_ns"], 53500)
        self.assertAlmostEqual(summary["scheduler_fraction"], 53500 / 77500)

    def test_phase_records_reject_missing_or_misaligned_counts(self):
        row = {
            "runtime": "nulang", "suite": "savina-style", "schema": 1,
            "benchmark": "fork_join", "iteration": 1, "messages": 100000,
            "elapsed_ns": 1000, "phase_admission_ns": 250,
            "phase_scheduler_ns": 750,
        }
        for updates in (
            {"phase_scheduler_ns": 900},
            {"phase_admission_ns": -1},
            {"messages": 99999},
            {"runtime": "ractor"},
            {"iteration": 2},
        ):
            with self.subTest(case=updates), self.assertRaises(RuntimeError):
                profile.validate_phase_samples(
                    json.dumps(dict(row, **updates)), "fork_join", 1, 100000
                )

    def test_refuses_invalid_workload(self):
        with self.assertRaisesRegex(ValueError, "workload"):
            profile.validate_options("unknown", 3, 99)

    def test_refuses_invalid_repetition_and_frequency(self):
        for args in [("ping_pong", 0, 99), ("ping_pong", 2, 0),
                     ("ping_pong", 2, 2000)]:
            with self.subTest(options=args):
                with self.assertRaises(ValueError):
                    profile.validate_options(*args)

    def test_builds_exact_non_durable_runner_command(self):
        command = profile.benchmark_command(
            Path("/tmp/nulang-savina"), "ping_pong", 3
        )
        self.assertEqual(command, [
            "/tmp/nulang-savina", "--benchmark", "ping_pong",
            "--repeat", "3", "--format", "jsonl", "--reuse-setup",
        ])

    def test_other_workloads_keep_existing_fresh_fixture_semantics(self):
        command = profile.benchmark_command(Path("/tmp/nulang-savina"), "fork_join", 3)
        self.assertNotIn("--reuse-setup", command)

    def test_parses_all_samples_with_contiguous_iterations(self):
        records = profile.parse_samples(
            "\n".join([row(iteration=1), row(iteration=2)]),
            "ping_pong", 2,
        )
        self.assertEqual([x["iteration"] for x in records], [1, 2])

    def test_does_not_accept_incomplete_samples(self):
        with self.assertRaisesRegex(RuntimeError, "sample count"):
            profile.parse_samples(row(), "ping_pong", 2)

    def test_does_not_accept_duplicate_iterations(self):
        with self.assertRaisesRegex(RuntimeError, "iteration"):
            profile.parse_samples("\n".join([row(), row()]), "ping_pong", 2)

    def test_does_not_accept_wrong_benchmark_or_runtime(self):
        with self.assertRaisesRegex(RuntimeError, "benchmark"):
            profile.parse_samples(row(benchmark="counting"), "ping_pong", 1)
        with self.assertRaisesRegex(RuntimeError, "runtime"):
            profile.parse_samples(row().replace('"nulang"', '"go"'), "ping_pong", 1)

    def test_rejects_non_positive_timing_and_message_count(self):
        for content in (row(elapsed_ns=0), row().replace('"messages": 40001', '"messages": 0')):
            with self.subTest(content=content):
                with self.assertRaisesRegex(RuntimeError, "positive"):
                    profile.parse_samples(content, "ping_pong", 1)

    def test_builds_perf_record_without_shell_interpolation(self):
        command = profile.perf_command(
            Path("/usr/bin/perf"), Path("/tmp/capture.data"),
            Path("/tmp/nulang-savina"), "ping_pong", 2, 99,
        )
        self.assertEqual(command, [
            "/usr/bin/perf", "record", "--call-graph", "fp",
            "--freq", "99", "--output", "/tmp/capture.data", "--",
            "/tmp/nulang-savina", "--benchmark", "ping_pong",
            "--repeat", "2", "--format", "jsonl", "--reuse-setup",
        ])


if __name__ == "__main__":
    unittest.main()
