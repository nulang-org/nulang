"""Smoke the size harness itself independently of the Nulang compiler build."""

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HARNESS = Path(__file__).resolve().parents[1] / "measure_native_binary_size.py"


class BinarySizeHarnessTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cc") and shutil.which("readelf"), "requires ELF host and cc")
    def test_baseline_reports_real_elf_and_linker_dependencies(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "report.json"
            done = subprocess.run(
                [sys.executable, str(HARNESS), "--baseline-only", "--json-out", str(output)],
                capture_output=True,
                text=True,
                timeout=180,
            )
            self.assertEqual(done.returncode, 0, done.stderr)
            report = json.loads(output.read_text())
            self.assertEqual(report["schema_version"], 1)
            self.assertEqual(report["workload"], "one_literal_stdout_line")
            self.assertEqual(report["expected_stdout"], "Hello from Nulang\n")
            c_artifact = next(a for a in report["artifacts"] if a["name"].startswith("C libc"))
            self.assertGreater(c_artifact["bytes"], 0)
            self.assertIsInstance(c_artifact["pt_interp"], bool)
            self.assertIsInstance(c_artifact["needed_libraries"], list)
            self.assertNotIn("Nulang experimental libc", [a["name"] for a in report["artifacts"]])


if __name__ == "__main__":
    unittest.main()
