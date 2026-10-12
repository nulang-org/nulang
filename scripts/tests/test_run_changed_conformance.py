"""Unit tests for selecting and executing changed Nulang behavior fixtures."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "run_changed_conformance.py"


class ChangedConformanceTests(unittest.TestCase):
    def setUp(self):
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.root = Path(self.tempdir.name)
        self.fixtures = self.root / "conformance" / "behavior"
        self.fixtures.mkdir(parents=True)
        self.binary = self.root / "fake_nulang.py"
        self.binary.write_text(
            "#!/usr/bin/env python3\n"
            "import pathlib, sys\n"
            "src = pathlib.Path(sys.argv[1]).read_text()\n"
            "if 'bad-output' in src: print('actual')\n"
            "elif 'nonzero' in src: print('failed', file=sys.stderr); sys.exit(3)\n"
            "else: print('expected')\n",
            encoding="utf-8",
        )
        self.binary.chmod(0o755)
        self.changed = self.root / "changed.txt"

    def fixture(self, stem="check", source="ok", expected="expected", exit_code=0):
        path = self.fixtures / (stem + ".nula")
        path.write_text(source, encoding="utf-8")
        path.with_suffix(".json").write_text(
            json.dumps({"exit_code": exit_code, "stdout": expected, "stderr": ""}),
            encoding="utf-8",
        )
        return path

    def run_runner(self, *paths):
        self.changed.write_text("".join(str(x.relative_to(self.root)) + "\n" for x in paths), encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(self.root),
             "--binary", str(self.binary), "--file-list", str(self.changed)],
            capture_output=True, text=True,
        )

    def test_exact_output_succeeds_and_empty_change_list_is_noop(self):
        p = self.fixture()
        result = self.run_runner(p)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 passed", result.stdout)
        self.assertEqual(self.run_runner().returncode, 0)

    def test_changed_fixture_failure_is_blocking(self):
        p = self.fixture(source="bad-output")
        result = self.run_runner(p)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAIL", result.stdout)

    def test_missing_snapshot_is_blocking(self):
        p = self.fixture()
        p.with_suffix(".json").unlink()
        result = self.run_runner(p)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing expectation", result.stdout)

    def test_nonzero_exit_code_is_checked(self):
        p = self.fixture(source="nonzero", expected="", exit_code=3)
        result = self.run_runner(p)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        p.with_suffix(".json").write_text(json.dumps({"exit_code": 0, "stdout": "", "stderr": ""}))
        self.assertNotEqual(self.run_runner(p).returncode, 0)

    def test_snapshot_only_change_runs_source_once(self):
        p = self.fixture()
        result = self.run_runner(p.with_suffix(".json"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 passed", result.stdout)

    def test_source_and_snapshot_change_deduplicates(self):
        p = self.fixture()
        result = self.run_runner(p, p.with_suffix(".json"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 passed, 0 failed", result.stdout)

    def test_deleted_snapshot_is_blocking(self):
        p = self.fixture()
        snapshot = p.with_suffix(".json")
        snapshot.unlink()
        result = self.run_runner(snapshot)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing expectation", result.stdout)

    def test_deleted_source_with_retained_snapshot_is_blocking(self):
        p = self.fixture()
        p.unlink()
        result = self.run_runner(p)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing fixture", result.stdout)

    def test_deleted_fixture_and_snapshot_pair_is_skipped(self):
        p = self.fixture()
        snapshot = p.with_suffix(".json")
        p.unlink()
        snapshot.unlink()
        result = self.run_runner(p, snapshot)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("SKIP", result.stdout)
        self.assertIn("0 passed, 0 failed, 1 skipped", result.stdout)

    def test_rejects_paths_outside_conformance_directory(self):
        p = self.root / "outside.nula"
        p.write_text("ok")
        result = self.run_runner(p)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside conformance/behavior", result.stdout)


if __name__ == "__main__":
    unittest.main()
