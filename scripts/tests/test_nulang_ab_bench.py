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


class ScalingHarnessTests(unittest.TestCase):
    def test_parse_scaling_records_keeps_each_shard_count_distinct(self):
        output = """
[scale-bench] benchmark=actor_drain shards=1 operations=200000 elapsed_ns=1000000
[scale-bench] benchmark=actor_drain shards=2 operations=200000 elapsed_ns=600000
[scale-bench] benchmark=actor_drain shards=4 operations=200000 elapsed_ns=350000
"""

        rows = nulang_ab_bench.parse_scaling_records(output)

        self.assertEqual(
            {"operations": 200000, "elapsed_ns": 600000},
            rows["actor_drain/shards_2"],
        )
        self.assertEqual(
            {"operations": 200000, "elapsed_ns": 350000},
            rows["actor_drain/shards_4"],
        )

    def test_scaling_command_targets_only_opt_in_scaling_probe(self):
        command = nulang_ab_bench.scaling_command(
            ["--no-default-features", "--features", "native-codegen"]
        )

        self.assertIn("benchmarks::bench_ab_shard_scaling", command)
        self.assertNotIn("benchmarks::bench_", command)

    def test_scaling_environment_enables_probe_without_losing_variant_isolation(self):
        env = nulang_ab_bench.scaling_environment("candidate")

        self.assertEqual("1", env["NULANG_BENCH_SHARD_SCALING"])
        self.assertIn("CARGO_TARGET_DIR", env)


if __name__ == "__main__":
    unittest.main()
