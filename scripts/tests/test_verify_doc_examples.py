"""Regression coverage for the documentation Nulang-code verifier.

Run: python3 -m unittest scripts.tests.test_verify_doc_examples
"""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


VERIFY_SCRIPT = Path(__file__).resolve().parents[1] / "verify_doc_examples.sh"
HELLO_EXAMPLE = 'perform IO.print("ok")'


class VerifyDocExamplesTests(unittest.TestCase):
    def run_verifier(
        self,
        extensions: tuple[str, ...],
        binary_exit_code: int = 0,
        source: str = HELLO_EXAMPLE,
    ):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            docs = root / "docs" / "src" / "content" / "docs" / "actors"
            docs.mkdir(parents=True)
            (root / "src").mkdir()
            for ext in extensions:
                (docs / f"sample.{ext}").write_text(
                    f"# Example\n\n```nulang\n{source}\n```\n",
                    encoding="utf-8",
                )

            call_log = root / "calls.txt"
            fake_nulang = root / "fake-nulang"
            fake_nulang.write_text(
                f'#!/bin/sh\nprintf "%s\\n" "$*" >> "$NULANG_TEST_LOG"\nexit {binary_exit_code}\n',
                encoding="utf-8",
            )
            fake_nulang.chmod(0o755)

            result = subprocess.run(
                ["bash", str(VERIFY_SCRIPT)],
                cwd=root,
                env={
                    **os.environ,
                    "NULANG_BIN": str(fake_nulang),
                    "NULANG_TEST_LOG": str(call_log),
                },
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            calls = call_log.read_text(encoding="utf-8").splitlines() if call_log.exists() else []
            return result, calls

    def test_verifies_both_markdown_and_mdx_examples(self):
        result, calls = self.run_verifier(("md", "mdx"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("actors/sample.md#1 (run)", result.stdout)
        self.assertIn("actors/sample.mdx#1 (run)", result.stdout)
        self.assertIn("2 passed, 0 failed", result.stdout)
        self.assertEqual(len(calls), 2)

    def test_markdown_compilation_failure_fails_verification(self):
        result, _ = self.run_verifier(("md",), binary_exit_code=1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FAIL  actors/sample.md#1", result.stdout)
        self.assertIn("0 passed, 1 failed", result.stdout)

    def test_tool_declarations_are_checked_without_running(self):
        result, calls = self.run_verifier(
            ("md",),
            source='@tool(description: "Add")\nfn add(x: Int, y: Int) -> Int { x + y }',
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1, calls)
        self.assertTrue(calls[0].startswith("--check "), calls)

    def test_interactive_input_is_checked_without_running(self):
        result, calls = self.run_verifier(
            ("md",),
            source='let input = perform IO.read()',
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1, calls)
        self.assertTrue(calls[0].startswith("--check "), calls)


if __name__ == "__main__":
    unittest.main()
