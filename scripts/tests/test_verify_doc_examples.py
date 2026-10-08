"""Regression coverage for docs example discovery and validation.

Run with: python3 -m unittest scripts.tests.test_verify_doc_examples
"""
from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


VERIFY_SCRIPT = Path(__file__).resolve().parents[1] / "verify_doc_examples.sh"


class VerifyDocExamplesTests(unittest.TestCase):
    def run_verifier(self, extensions: tuple[str, ...], binary_exit_code: int = 0):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            docs = root / "docs" / "src" / "content" / "docs" / "actors"
            docs.mkdir(parents=True)
            (root / "src").mkdir()
            for ext in extensions:
                (docs / f"sample.{ext}").write_text(
                    '# Example\n\n```nulang\nperform IO.print("ok")\n```\n',
                    encoding="utf-8",
                )

            fake_nulang = root / "fake-nulang"
            fake_nulang.write_text(
                f"#!/bin/sh\nexit {binary_exit_code}\n", encoding="utf-8"
            )
            fake_nulang.chmod(0o755)

            return subprocess.run(
                ["bash", str(VERIFY_SCRIPT)],
                cwd=root,
                env={**os.environ, "NULANG_BIN": str(fake_nulang)},
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )

    def test_verifies_both_markdown_and_mdx_examples(self):
        result = self.run_verifier(("md", "mdx"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("actors/sample.md#1 (run)", result.stdout)
        self.assertIn("actors/sample.mdx#1 (run)", result.stdout)
        self.assertIn("2 passed, 0 failed", result.stdout)

    def test_markdown_compilation_failure_fails_verification(self):
        result = self.run_verifier(("md",), binary_exit_code=1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("FAIL  actors/sample.md#1", result.stdout)
        self.assertIn("0 passed, 1 failed", result.stdout)


if __name__ == "__main__":
    unittest.main()
