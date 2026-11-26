"""Fail closed on malformed or incomparable cross-runtime benchmark evidence."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "cross_runtime_bench.py"
SPEC = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)

COUNTS = {
    "counting": 200_000,
    "ping_pong": 40_001,
    "thread_ring": 20_000,
    "fork_join": 100_000,
}


def record(runtime, name, count, elapsed=123_456):
    return (
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={count} elapsed_ns={elapsed}"
    )


def fixture(runtime="go"):
    return "\n".join(record(runtime, name, count) for name, count in COUNTS.items())


class CrossRuntimeEvidenceTests(unittest.TestCase):
    def test_valid_records_for_all_four_runtimes(self):
        for runtime in ("nulang", "rust", "go", "erlang"):
            with self.subTest(runtime=runtime):
                result = bench.parse_records(
                    "ordinary diagnostic\n" + fixture(runtime) + "\n", runtime
                )
                self.assertEqual(
                    result,
                    {
                        name: {"messages": count, "elapsed_ns": 123_456}
                        for name, count in COUNTS.items()
                    },
                )

    def test_nulang_only_skynet_is_validated_but_not_compared(self):
        skynet = record("nulang", "skynet", 1_110)
        output = fixture("nulang") + "\n" + skynet
        self.assertEqual(set(bench.parse_records(output, "nulang")), set(COUNTS))
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_records(output + "\n" + skynet, "nulang")
        with self.assertRaisesRegex(RuntimeError, "message count"):
            bench.parse_records(output.replace("messages=1110", "messages=1111"), "nulang")
        with self.assertRaisesRegex(RuntimeError, "benchmark"):
            bench.parse_records(fixture() + "\n" + record("go", "skynet", 1_110), "go")

    def test_invalid_sample_prevents_report_publication(self):
        good = fixture()
        bad = good.replace("messages=200000", "messages=200001")
        for outputs in ([bad], [good, bad]):
            with self.subTest(outputs=len(outputs)), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "report.json"
                argv = [
                    str(SCRIPT), "--runtimes", "go", "--cpu-mode", "host",
                    "--runs", "1", "--warmup", "1", "--output", str(path),
                ]
                with (
                    mock.patch.object(sys, "argv", argv),
                    mock.patch.object(bench, "build_commands", return_value={"go": ["fake"]}),
                    mock.patch.object(bench, "command_output", side_effect=outputs),
                    self.assertRaisesRegex(RuntimeError, "message count"),
                ):
                    bench.main()
                self.assertFalse(path.exists())

    def test_duplicate_record_is_rejected_even_if_values_match(self):
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_records(fixture() + "\n" + record("go", "counting", 200_000), "go")

    def test_wrong_counts_are_rejected_in_each_workload(self):
        for name, count in COUNTS.items():
            with self.subTest(benchmark=name):
                output = fixture().replace(record("go", name, count), record("go", name, count + 1))
                with self.assertRaisesRegex(RuntimeError, "message count"):
                    bench.parse_records(output, "go")

    def test_zero_duration_is_rejected(self):
        output = fixture().replace(record("go", "counting", 200_000), record("go", "counting", 200_000, 0))
        with self.assertRaisesRegex(RuntimeError, "positive"):
            bench.parse_records(output, "go")

    def test_foreign_runtime_record_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "runtime"):
            bench.parse_records(fixture() + "\n" + record("rust", "counting", 200_000), "go")

    def test_unknown_workload_record_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "benchmark"):
            bench.parse_records(fixture() + "\n" + record("go", "invented", 200_000), "go")

    def test_malformed_marker_is_not_ignored(self):
        for suffix in (" elapsed_ns=9", " trailing junk", ".5"):
            with self.subTest(suffix=suffix):
                with self.assertRaisesRegex(RuntimeError, "malformed"):
                    bench.parse_records(fixture().replace(record("go", "counting", 200_000), record("go", "counting", 200_000) + suffix), "go")

    def test_missing_workload_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "fork_join"):
            bench.parse_records("\n".join(fixture().splitlines()[:-1]), "go")

    def test_runtime_selection_cannot_be_empty_or_duplicate(self):
        for selection in ("", ",,", "go,go"):
            with self.subTest(selection=selection):
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), "--runtimes", selection],
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("--runtimes", result.stderr)


if __name__ == "__main__":
    unittest.main()
