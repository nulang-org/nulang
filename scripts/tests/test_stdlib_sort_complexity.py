#!/usr/bin/env python3
"""Regression coverage for the stdlib sorting comparator budget.

Run with NULANG_BIN=/path/to/nulang python3 scripts/tests/test_stdlib_sort_complexity.py
Uses real Nulang compilation and execution; no VM mocks.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def compiler_binary() -> Path:
    override = os.environ.get("NULANG_BIN")
    if override:
        return Path(override).resolve()
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        text=True,
    )
    return Path(json.loads(metadata)["target_directory"]) / "debug" / "nulang"


class ListSortComplexityTests(unittest.TestCase):
    def verify_sort(self, source_expression: str, first: int, last: int, budget: int):
        source = f"""import stdlib::list
fn main() {{
    let cmp = fn(a, b) {{
        perform IO.print("comparison");
        a < b
    }};
    let sorted = sort_by(cmp, {source_expression});
    perform IO.print("first=" + perform Int.to_string(sorted[0]));
    perform IO.print("last=" + perform Int.to_string(sorted[63]));
    perform IO.print("length=" + perform Int.to_string(length(sorted)))
}}
"""
        with tempfile.TemporaryDirectory(prefix="nulang-sort-") as tmp:
            path = Path(tmp) / "sort.nula"
            path.write_text(source, encoding="utf-8")
            proc = subprocess.run(
                [str(compiler_binary()), str(path)],
                cwd=ROOT,
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        output = proc.stdout.splitlines()
        self.assertIn(f"first={first}", output)
        self.assertIn(f"last={last}", output)
        self.assertIn("length=64", output)
        self.assertLess(
            output.count("comparison"),
            budget,
            f"Comparator calls exceeded budget for {source_expression}",
        )

    def test_sorted_input_does_not_cause_quadratic_comparisons(self):
        self.verify_sort("range(64)", first=0, last=63, budget=900)

    def test_reversed_input_does_not_cause_quadratic_comparisons(self):
        self.verify_sort("reverse(range(64))", first=0, last=63, budget=900)

    def test_equal_values_do_not_repeat_comparisons_unnecessarily(self):
        self.verify_sort("map(fn(x) { 7 }, range(64))", first=7, last=7, budget=129)


if __name__ == "__main__":
    unittest.main()
