"""Executable integration gate for combined Experimental list helpers.

The fixture oracle is the same JSON used by the Nulang conformance runner,
but only the affected list fixtures are selected. This avoids treating known
unrelated behavioral-suite failures as evidence of list regressions.
"""

import json
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = (
    "stdlib_07_list_sort_sort_by",
    "stdlib_19_list_combinators",
    "stdlib_20_list_sort_balanced",
    "stdlib_24_list_polymorphic_combinators",
    "stdlib_25_list_combined_integration",
)


def nulang_binary():
    binary = os.environ.get("NULANG_BIN")
    if binary:
        return Path(binary).resolve()
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        text=True,
    )
    target = Path(json.loads(metadata)["target_directory"])
    return target / "debug" / "nulang"


class ListIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = nulang_binary()
        if not cls.binary.is_file():
            raise RuntimeError(f"Nulang CLI must be built first: {cls.binary}")

    def test_cross_pr_behavioral_fixtures(self):
        for case in FIXTURES:
            with self.subTest(case=case):
                file = ROOT / "conformance" / "behavior" / f"{case}.nula"
                expected = json.loads(file.with_suffix(".json").read_text())
                process = subprocess.run(
                    [str(self.binary), str(file.relative_to(ROOT))],
                    cwd=ROOT,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                self.assertEqual(process.returncode, expected.get("exit_code", 0), process.stderr)
                self.assertEqual(process.stdout.strip(), expected["stdout"])
                if expected.get("stderr"):
                    self.assertIn(expected["stderr"], process.stderr)

    def test_nulang_authored_list_unit_suite(self):
        process = subprocess.run(
            [str(self.binary), "src/stdlib/list_test.nula"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)


if __name__ == "__main__":
    unittest.main()
