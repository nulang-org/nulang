"""Fail-closed subprocess diagnostics and toolchain contract for cross-runtime CI."""

import contextlib
import importlib.util
import io
import pathlib
import subprocess
import sys
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "cross_runtime_bench.py"
WORKFLOW = ROOT / ".github" / "workflows" / "cross-runtime-bench.yml"

spec = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
cross_runtime_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cross_runtime_bench)


class CrossRuntimeCommandTests(unittest.TestCase):
    def test_failed_benchmark_command_prints_captured_diagnostics(self):
        failure = subprocess.CalledProcessError(
            1, ["go", "build"], output="go: compiler failed on exact head\n"
        )
        stderr = io.StringIO()
        with mock.patch.object(
            cross_runtime_bench.subprocess, "run", side_effect=failure
        ):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(subprocess.CalledProcessError):
                    cross_runtime_bench.command_output(["go", "build"])

        self.assertIn("compiler failed on exact head", stderr.getvalue())

    def test_failed_optional_version_probe_is_quiet(self):
        failure = subprocess.CalledProcessError(
            1, ["missing", "--version"], output="not installed\n"
        )
        stderr = io.StringIO()
        with mock.patch.object(
            cross_runtime_bench.subprocess, "run", side_effect=failure
        ):
            with contextlib.redirect_stderr(stderr):
                version = cross_runtime_bench.maybe_version(["missing", "--version"])

        self.assertIsNone(version)
        self.assertEqual("", stderr.getvalue())

    def test_measurement_timeout_is_forwarded_to_child(self):
        success = subprocess.CompletedProcess(
            ["bench"], 0, stdout="timing record\n"
        )
        with mock.patch.object(
            cross_runtime_bench.subprocess, "run", return_value=success
        ) as runner:
            result = cross_runtime_bench.command_output(
                ["bench"], timeout_seconds=2.5
            )

        self.assertEqual("timing record\n", result)
        self.assertEqual(2.5, runner.call_args.kwargs["timeout"])

    def test_timeout_prints_partial_output_and_re_raises(self):
        timeout = subprocess.TimeoutExpired(
            ["bench"], 0.5, output=b"last successful actor turn\n"
        )
        stderr = io.StringIO()
        with mock.patch.object(
            cross_runtime_bench.subprocess, "run", side_effect=timeout
        ):
            with contextlib.redirect_stderr(stderr):
                with self.assertRaises(subprocess.TimeoutExpired):
                    cross_runtime_bench.command_output(
                        ["bench"], timeout_seconds=0.5
                    )

        self.assertIn("last successful actor turn", stderr.getvalue())
        self.assertIn("timed out", stderr.getvalue())

    def test_version_probe_timeout_is_bounded_quiet_and_nonfatal(self):
        timeout = subprocess.TimeoutExpired(
            ["hung-tool", "--version"], 5.0, output=b"partial tool output\\n"
        )
        stderr = io.StringIO()
        with mock.patch.object(
            cross_runtime_bench.subprocess, "run", side_effect=timeout
        ) as runner:
            with contextlib.redirect_stderr(stderr):
                version = cross_runtime_bench.maybe_version(["hung-tool", "--version"])

        self.assertIsNone(version)
        self.assertEqual("", stderr.getvalue())
        self.assertGreater(runner.call_args.kwargs["timeout"], 0)
        self.assertLessEqual(runner.call_args.kwargs["timeout"], 10.0)

    def test_non_finite_timeout_is_rejected_before_building(self):
        for value in ("nan", "inf", "0", "-1"):
            with self.subTest(value=value):
                with mock.patch.object(
                    sys,
                    "argv",
                    ["cross_runtime_bench.py", "--measurement-timeout", value],
                ), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as raised:
                        cross_runtime_bench.main()
                self.assertEqual(2, raised.exception.code)


class GoBaselineContractTests(unittest.TestCase):
    def test_workflow_uses_current_supported_go_toolchain(self):
        self.assertIn("go-version: '1.27.x'", WORKFLOW.read_text())

    def test_workflow_runs_diagnostics_tests_before_expensive_build(self):
        workflow = WORKFLOW.read_text()
        self.assertIn(
            "python3 -m unittest scripts/tests/test_cross_runtime_command.py",
            workflow,
        )
        self.assertLess(
            workflow.index("test_cross_runtime_command.py"),
            workflow.index("- name: Install Rust"),
        )


if __name__ == "__main__":
    unittest.main()
