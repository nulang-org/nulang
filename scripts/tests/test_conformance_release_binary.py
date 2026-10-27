"""Qualify the exact supplied CLI binary, not a separately built debug executable."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("nulang_release_conformance", ROOT / "conformance" / "run.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class ReleaseBinaryQualificationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.binary = self.directory / "candidate-nulang"
        self.binary.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "print('hello')\n"
            "sys.exit(0)\n",
            encoding="utf-8",
        )
        self.binary.chmod(0o755)

    def fixture(self, output="hello"):
        behavior = self.directory / "behavior"
        behavior.mkdir(exist_ok=True)
        (behavior / "core_01.nula").write_text("0", encoding="utf-8")
        (behavior / "core_01.json").write_text(
            json.dumps({"stdout": output}), encoding="utf-8"
        )
        return behavior

    def test_explicit_binary_runs_all_fixtures_without_invoking_cargo(self):
        behavior = self.fixture()
        original_run = subprocess.run

        def prohibit_cargo(command, *args, **kwargs):
            if command[0] == "cargo":
                raise AssertionError("explicit release binary must not invoke cargo")
            return original_run(command, *args, **kwargs)

        with mock.patch.object(runner, "BEHAVIOR", behavior):
            with mock.patch.object(runner.subprocess, "run", side_effect=prohibit_cargo):
                with contextlib.redirect_stdout(io.StringIO()):
                    code = runner.main(["--binary", str(self.binary)])
        self.assertEqual(code, 0)

    def test_omitting_binary_retains_existing_debug_build_path(self):
        behavior = self.fixture()
        debug_dir = self.directory / "debug"
        debug_dir.mkdir()
        debug_binary = debug_dir / "nulang"
        debug_binary.write_text(self.binary.read_text(encoding="utf-8"), encoding="utf-8")
        debug_binary.chmod(0o755)
        real_run = subprocess.run
        cargo_builds = []

        def intercept_cargo(command, *args, **kwargs):
            if command[:2] == ["cargo", "build"]:
                cargo_builds.append(command)
                return subprocess.CompletedProcess(command, 0)
            return real_run(command, *args, **kwargs)

        with mock.patch.object(runner, "BEHAVIOR", behavior):
            with mock.patch.object(runner.subprocess, "run", side_effect=intercept_cargo):
                with mock.patch.object(
                    runner.subprocess,
                    "check_output",
                    return_value=json.dumps({"target_directory": str(self.directory)}).encode(),
                ):
                    with contextlib.redirect_stdout(io.StringIO()):
                        code = runner.main([])
        self.assertEqual(code, 0)
        self.assertEqual(len(cargo_builds), 1)

    def test_missing_supplied_binary_does_not_fall_back_to_debug_binary(self):
        behavior = self.fixture()
        with mock.patch.object(runner, "BEHAVIOR", behavior):
            with mock.patch.object(runner.subprocess, "run") as cargo:
                with contextlib.redirect_stderr(io.StringIO()):
                    code = runner.main(["--binary", str(self.directory / "missing")])
        self.assertEqual(code, 1)
        cargo.assert_not_called()

    def test_conformance_failure_still_blocks_release_qualification(self):
        behavior = self.fixture(output="this is not hello")
        with mock.patch.object(runner, "BEHAVIOR", behavior):
            with contextlib.redirect_stdout(io.StringIO()):
                code = runner.main(["--binary", str(self.binary)])
        self.assertEqual(code, 1)

    def test_empty_fixture_set_blocks_before_binary_execution(self):
        behavior = self.directory / "empty"
        behavior.mkdir()
        with mock.patch.object(runner, "BEHAVIOR", behavior):
            with mock.patch.object(runner.subprocess, "run") as execute:
                with contextlib.redirect_stderr(io.StringIO()):
                    code = runner.main(["--binary", str(self.binary)])
        self.assertEqual(code, 1)
        execute.assert_not_called()


if __name__ == "__main__":
    unittest.main()
