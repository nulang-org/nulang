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


class CargoCommandTests(unittest.TestCase):
    def test_benchmark_commands_compile_only_library_test_target(self):
        for command in (
            nulang_ab_bench.cargo_build_command([]),
            nulang_ab_bench.cargo_command([]),
        ):
            self.assertIn("--lib", command)
            self.assertLess(command.index("--lib"), command.index("benchmarks::bench_"))


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
