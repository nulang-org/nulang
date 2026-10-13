"""Non-vacuity and fail-closed behavior for conformance/run.py (no Rust required)."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("nulang_conformance_runner", ROOT / "conformance" / "run.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class DiscoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def write(self, stem, extension, content=""):
        (self.directory / f"{stem}.{extension}").write_text(content, encoding="utf-8")

    def test_empty_suite_is_error(self):
        with self.assertRaisesRegex(runner.SuiteError, "no .*fixtures"):
            runner.discover_fixtures(self.directory)

    def test_expected_result_without_source_is_error(self):
        self.write("lost", "json", "{}")
        with self.assertRaisesRegex(runner.SuiteError, "orphan .*json.*lost"):
            runner.discover_fixtures(self.directory)

    def test_main_rejects_empty_suite_before_cargo_build(self):
        with mock.patch.object(runner, "BEHAVIOR", self.directory):
            with mock.patch.object(runner.subprocess, "run") as build:
                self.assertEqual(runner.main(), 1)
                build.assert_not_called()

    def test_source_without_expected_result_is_error(self):
        self.write("lost", "nula", "42")
        with self.assertRaisesRegex(runner.SuiteError, "missing .*json.*lost"):
            runner.discover_fixtures(self.directory)

    def test_pairing_is_sorted_and_complete(self):
        for stem in ("zeta", "alpha"):
            self.write(stem, "nula", "0")
            self.write(stem, "json", "{}")
        pairs = runner.discover_fixtures(self.directory)
        self.assertEqual([n.stem for n, _ in pairs], ["alpha", "zeta"])
        self.assertEqual([j.stem for _, j in pairs], ["alpha", "zeta"])


class ExecutionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.executable = self.directory / "fake-nulang"
        self.executable.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "from pathlib import Path\n"
            "name = Path(sys.argv[1]).stem\n"
            "if name == 'unexpected':\n"
            "  print('different'); sys.exit(0)\n"
            "if name == 'runtime_failure':\n"
            "  print('crash', file=sys.stderr); sys.exit(2)\n"
            "if name == 'slow':\n"
            "  import time; time.sleep(2); sys.exit(0)\n"
            "print('hello'); sys.exit(0)\n",
            encoding="utf-8",
        )
        self.executable.chmod(0o755)

    def fixture(self, name, expected):
        source = self.directory / (name + ".nula")
        specification = self.directory / (name + ".json")
        source.write_text("// fake case", encoding="utf-8")
        specification.write_text(json.dumps(expected), encoding="utf-8")
        return [(source, specification)]

    def test_successful_fixture_is_counted(self):
        fixtures = self.fixture("good", {"stdout": "hello"})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (1, 0))

    def test_output_mismatch_is_failure(self):
        fixtures = self.fixture("unexpected", {"stdout": "hello"})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (0, 1))

    def test_unexpected_nonzero_exit_is_failure(self):
        fixtures = self.fixture("runtime_failure", {"stdout": ""})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (0, 1))

    def test_invalid_expectation_schema_is_failure(self):
        fixtures = self.fixture("good", {"exit_code": "0", "stdout": "hello"})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (0, 1))

    def test_explicit_expected_failure_and_stderr_substring_passes(self):
        fixtures = self.fixture("runtime_failure", {"exit_code": 2, "stderr": "crash"})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (1, 0))

    def test_malformed_json_is_failure(self):
        fixtures = self.fixture("good", {"stdout": "hello"})
        fixtures[0][1].write_text("not json", encoding="utf-8")
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory), (0, 1))

    def test_timeout_is_failure(self):
        fixtures = self.fixture("slow", {"stdout": "hello"})
        self.assertEqual(runner.run_suite(self.executable, fixtures, self.directory, timeout_seconds=0.05), (0, 1))


if __name__ == "__main__":
    unittest.main()
