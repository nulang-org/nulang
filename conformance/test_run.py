"""Isolated, deterministic contract checks for the Nulang conformance runner."""

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from conformance import run


class ConformanceIsolationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="nulang-conformance-runner-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_each_case_has_a_distinct_store(self):
        first = run.case_command("nulang", self.root / "one.nula", {}, self.root)
        second = run.case_command("nulang", self.root / "two.nula", {}, self.root)
        self.assertIn("--store", first)
        self.assertNotEqual(first[first.index("--store") + 1], second[second.index("--store") + 1])
        self.assertEqual(first[-1], str(self.root / "one.nula"))

    def test_explicit_fs_capabilities_preserved(self):
        command = run.case_command("nulang", self.root / "fs.nula", {"capabilities": ["fs"]}, self.root)
        self.assertEqual(command[command.index("--with") + 1], "fs")
        self.assertIn("--store", command)

    def test_case_without_capabilities_has_no_escalation(self):
        command = run.case_command("nulang", self.root / "pure.nula", {}, self.root)
        self.assertNotIn("--with", command)

    def test_invalid_capability_rejected(self):
        with self.assertRaises(ValueError):
            run.case_command("nulang", self.root / "bad.nula", {"capabilities": ["admin"]}, self.root)

    def test_store_directory_cannot_escape_root(self):
        with self.assertRaises(ValueError):
            run.case_command("nulang", self.root / ".." / "escaped.nula", {}, self.root)

    def test_real_case_processes_cannot_share_durable_data(self):
        fake_binary = self.root / "fake-nulang"
        fake_binary.write_text(
            "#!/usr/bin/env python3\n"
            "import pathlib, sys\n"
            "argv = sys.argv[1:]\n"
            "store = pathlib.Path(argv[argv.index('--store') + 1])\n"
            "store.mkdir(parents=True, exist_ok=True)\n"
            "flag = store / 'previous-run'\n"
            "print('dirty' if flag.exists() else 'clean')\n"
            "flag.write_text('used')\n"
        )
        fake_binary.chmod(0o700)
        for name in ("first", "second"):
            (self.root / f"{name}.nula").write_text("// synthetic case\n")
            (self.root / f"{name}.json").write_text(
                json.dumps({"exit_code": 0, "stdout": "clean"})
            )
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            passed, failed = run.run_suite(str(fake_binary), self.root)
        self.assertEqual((passed, failed), (2, 0), output.getvalue())

    def test_store_paths_are_disposable(self):
        with tempfile.TemporaryDirectory(prefix="nulang-case-store-") as store:
            cmd = run.case_command("nulang", self.root / "ok.nula", {}, Path(store))
            location = Path(cmd[cmd.index("--store") + 1])
            self.assertTrue(location.is_relative_to(store))
        self.assertFalse(Path(store).exists())


if __name__ == "__main__":
    unittest.main()
