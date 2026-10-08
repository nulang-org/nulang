"""Integration contract for cross-runtime round counterbalancing."""

from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
from collections import Counter
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import cross_runtime_bench


class CrossRuntimeExecutionOrderTests(unittest.TestCase):
    def test_main_counterbalances_samples_and_records_actual_order(self):
        runtimes = ["nulang", "rust", "go", "erlang"]
        commands = {runtime: [runtime] for runtime in runtimes}
        observed = []

        def fake_command_output(command, **_kwargs):
            runtime = command[0]
            observed.append(runtime)
            return "\n".join(
                f"[cross-bench] runtime={runtime} benchmark={benchmark} "
                "messages=200 elapsed_ns=10000"
                for benchmark in cross_runtime_bench.BENCHMARKS
            )

        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "comparison.json"
            argv = [
                "cross_runtime_bench.py",
                "--runs", "8",
                "--warmup", "0",
                "--cpu-mode", "host",
                "--output", str(report_path),
            ]
            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(cross_runtime_bench, "build_commands", return_value=commands),
                mock.patch.object(cross_runtime_bench, "command_output", side_effect=fake_command_output),
                mock.patch.object(cross_runtime_bench, "environment_metadata", return_value={}),
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(cross_runtime_bench.main(), 0)

            rounds = [observed[i:i + 4] for i in range(0, len(observed), 4)]
            self.assertEqual(len(rounds), 8)
            self.assertEqual(rounds[0], runtimes)
            self.assertEqual(rounds[1], list(reversed(runtimes)))
            for runtime in runtimes:
                positions = Counter(round_.index(runtime) for round_ in rounds)
                self.assertEqual(positions, Counter({0: 2, 1: 2, 2: 2, 3: 2}))

            report = json.loads(report_path.read_text())
            self.assertEqual(report["measured_execution_orders"], rounds)
            for runtime in runtimes:
                self.assertEqual(
                    report["summary"][runtime]["counting"]["samples"], 8
                )


if __name__ == "__main__":
    unittest.main()
