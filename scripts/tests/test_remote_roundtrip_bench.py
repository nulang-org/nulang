import importlib.util
import pathlib
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "remote_roundtrip_bench.py"


class RemoteRoundTripBenchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("remote_roundtrip_bench", SCRIPT)
        if spec is None or spec.loader is None:
            raise RuntimeError("failed to load remote_roundtrip_bench.py")
        cls.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.module)

    def test_parse_record_accepts_matched_result(self):
        output = (
            "noise\n"
            "[remote-roundtrip] runtime=nulang iteration=1 "
            "roundtrips=10000 elapsed_ns=25000000 ns_per_roundtrip=2500.0\n"
        )
        row = self.module.parse_record(output, "nulang", 10000)
        self.assertEqual(10000, row["roundtrips"])
        self.assertEqual(25000000, row["elapsed_ns"])

    def test_parse_record_rejects_changed_operation_count(self):
        output = (
            "[remote-roundtrip] runtime=go iteration=1 "
            "roundtrips=9999 elapsed_ns=1 ns_per_roundtrip=0.0\n"
        )
        with self.assertRaisesRegex(RuntimeError, "roundtrip count"):
            self.module.parse_record(output, "go", 10000)

    def test_counterbalanced_order_reverses_every_other_round(self):
        runtimes = ["nulang", "go", "erlang"]
        self.assertEqual(runtimes, self.module.runtime_order(runtimes, 0))
        self.assertEqual(list(reversed(runtimes)), self.module.runtime_order(runtimes, 1))
        self.assertEqual(runtimes, self.module.runtime_order(runtimes, 2))

    def test_summary_uses_median_latency(self):
        samples = {
            "nulang": [
                {"roundtrips": 10, "elapsed_ns": 100},
                {"roundtrips": 10, "elapsed_ns": 300},
                {"roundtrips": 10, "elapsed_ns": 200},
            ]
        }
        summary = self.module.summarize(samples)
        self.assertEqual(200, summary["nulang"]["median_elapsed_ns"])
        self.assertEqual(20.0, summary["nulang"]["median_ns_per_roundtrip"])

    def test_erlang_baseline_uses_compiled_module_launcher(self):
        def which(name):
            return f"/usr/bin/{name}" if name in {"erl", "erlc"} else None

        with mock.patch.object(self.module.shutil, "which", side_effect=which), mock.patch.object(
            self.module, "command_output", return_value=""
        ) as command_output:
            commands = self.module.build_commands(["erlang"], 1000, 100)

        compile_command = command_output.call_args_list[0].args[0]
        self.assertEqual("/usr/bin/erlc", compile_command[0])
        self.assertIn("remote_roundtrip_baseline.erl", compile_command[-1])
        self.assertEqual("/usr/bin/erl", commands["erlang"][0])
        self.assertIn(
            "remote_roundtrip_baseline:run(1000, 100)",
            " ".join(commands["erlang"]),
        )
        self.assertNotIn("escript", " ".join(commands["erlang"]))


if __name__ == "__main__":
    unittest.main()
