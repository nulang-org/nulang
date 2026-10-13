import importlib.util
import pathlib
import subprocess
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "cross_runtime_bench.py"
spec = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


def records(runtime="erlang_otp", overrides=None):
    values = {
        "counting": (200000, 2000000),
        "ping_pong": (40001, 3000000),
        "thread_ring": (20000, 4000000),
        "fork_join": (100000, 5000000),
    }
    values.update(overrides or {})
    return "\n".join(
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={messages} elapsed_ns={elapsed}"
        for name, (messages, elapsed) in values.items()
    )


class RecordValidationTests(unittest.TestCase):
    def test_all_actor_framework_records_are_accepted(self):
        rows = bench.parse_records(records(), "erlang_otp")
        self.assertEqual(list(bench.BENCHMARKS), list(rows))
        self.assertEqual(40001, rows["ping_pong"]["messages"])

    def test_missing_benchmark_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "did not emit records"):
            bench.parse_records(
                records().replace("benchmark=fork_join", "benchmark=unknown"),
                "erlang_otp",
            )

    def test_duplicate_measurement_is_rejected(self):
        line = records().splitlines()[0]
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_records(records() + "\n" + line, "erlang_otp")

    def test_wrong_logical_count_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "message count"):
            bench.parse_records(
                records(overrides={"ping_pong": (40000, 3000000)}),
                "erlang_otp",
            )

    def test_zero_duration_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "elapsed_ns"):
            bench.parse_records(
                records(overrides={"thread_ring": (20000, 0)}),
                "erlang_otp",
            )

    def test_ignores_other_runtime_records(self):
        output = records(runtime="erlang") + "\n" + records()
        self.assertEqual(4, len(bench.parse_records(output, "erlang_otp")))

    def test_rejects_duplicate_selected_runtime(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            bench.validate_runtimes(["nulang", "erlang_otp", "erlang_otp"])

    def test_summarize_rejects_zero_elapsed_ns(self):
        data = {"erlang_otp": {"counting": [{"messages": 200000, "elapsed_ns": 0}]}}
        with self.assertRaisesRegex(RuntimeError, "elapsed_ns"):
            bench.summarize(data)


class BenchmarkProcessTimeoutTests(unittest.TestCase):
    def test_measured_process_timeout_is_forwarded_to_subprocess(self):
        completed = subprocess.CompletedProcess(["fixture"], 0, stdout="ok")
        with mock.patch.object(bench.subprocess, "run", return_value=completed) as runner:
            self.assertEqual(
                "ok",
                bench.command_output(["fixture"], timeout_seconds=75),
            )
        self.assertEqual(75, runner.call_args.kwargs["timeout"])


class ErlangOtpBuildPlanTests(unittest.TestCase):
    def test_otp_requires_erlang_toolchain(self):
        with mock.patch.object(bench.shutil, "which", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "erl and erlc"):
                bench.build_commands(["erlang_otp"])

    def test_otp_compiles_gen_server_fixture_and_launches_main(self):
        with mock.patch.object(bench.shutil, "which", side_effect=lambda cmd: "/usr/bin/" + cmd):
            with mock.patch.object(bench, "command_output") as output:
                commands = bench.build_commands(["erlang_otp"])
        compiled = output.call_args.args[0]
        self.assertIn("cross_runtime_otp.erl", " ".join(compiled))
        self.assertEqual("erl", pathlib.Path(commands["erlang_otp"][0]).name)
        self.assertIn("cross_runtime_otp", commands["erlang_otp"])

    def test_pony_requires_ponyc(self):
        with mock.patch.object(bench.shutil, "which", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "ponyc"):
                bench.build_commands(["pony"])

    def test_caf_requires_cmake(self):
        with mock.patch.object(bench.shutil, "which", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "cmake"):
                bench.build_commands(["caf"])

    def test_optional_actor_fixtures_build_only_when_explicitly_selected(self):
        with mock.patch.object(bench.shutil, "which", side_effect=lambda name: "/usr/bin/" + name):
            with mock.patch.object(bench, "command_output") as output:
                commands = bench.build_commands(["pony", "caf"])
        self.assertIn("pony", commands)
        self.assertIn("caf", commands)
        builds = " ".join(str(arg) for call in output.call_args_list
                          for arg in call.args[0])
        self.assertIn("pony_baseline", builds)
        self.assertIn("caf_baseline", builds)

    def test_validate_runtimes_includes_opt_in_pony_and_caf(self):
        self.assertEqual(["pony", "caf"], bench.validate_runtimes(["pony", "caf"]))
        with self.assertRaisesRegex(ValueError, "unknown runtimes"):
            bench.validate_runtimes(["actix"])


if __name__ == "__main__":
    unittest.main()
