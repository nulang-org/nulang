"""Regression tests for conformance runner fixture selection (no Rust build)."""

import contextlib
import io
import pathlib
import runpy
import subprocess
import sys
import unittest
from types import SimpleNamespace
from unittest.mock import patch


ROOT = pathlib.Path(__file__).resolve().parents[2]
RUNNER = ROOT / "conformance" / "run.py"


class ConformanceSelectionTests(unittest.TestCase):
    def _invoke(self, prefix, stdout):
        output = io.StringIO()
        errors = io.StringIO()
        fake_result = SimpleNamespace(returncode=0, stdout=stdout, stderr="")
        with (
            patch.object(
                sys,
                "argv",
                [str(RUNNER), *(prefix if isinstance(prefix, (list, tuple)) else [prefix])],
            ),
            patch("os.chdir"),
            patch("subprocess.run", return_value=fake_result) as execute,
            patch("subprocess.check_output", return_value=b'{"target_directory":"/tmp"}'),
            contextlib.redirect_stdout(output),
            contextlib.redirect_stderr(errors),
        ):
            try:
                runpy.run_path(str(RUNNER), run_name="__main__")
            except SystemExit as exit_status:
                return exit_status.code, output.getvalue(), errors.getvalue(), execute.call_count
        return 0, output.getvalue(), errors.getvalue(), execute.call_count

    def test_nonmatching_prefix_fails_before_build(self):
        code, output, errors, calls = self._invoke(
            "prefix_that_must_not_exist_9999", ""
        )
        self.assertEqual(code, 1)
        self.assertIn("no conformance fixtures", errors.lower())
        self.assertEqual(calls, 0, "empty selection must not trigger cargo build")

    def test_each_requested_prefix_must_match(self):
        code, _, errors, calls = self._invoke(
            ["effrow_01_closed_empty_row_pure", "missing_fixture_prefix_xyz"], "5"
        )
        self.assertEqual(code, 1)
        self.assertIn("missing_fixture_prefix_xyz", errors)
        self.assertEqual(calls, 0, "a missing prefix must fail before cargo build")

    def test_existing_prefix_selects_a_real_fixture(self):
        code, output, errors, calls = self._invoke(
            "effrow_01_closed_empty_row_pure", "5"
        )
        self.assertEqual(code, 0, errors)
        self.assertIn("1 passed, 0 failed", output)
        self.assertEqual(calls, 2, "cargo build and one Nulang fixture")


if __name__ == "__main__":
    unittest.main()
