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



if __name__ == "__main__":
    unittest.main()
