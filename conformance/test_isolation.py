"""Regression tests for hermetic conformance durable storage."""
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from isolation import run_isolated


class DurableStoreIsolationTests(unittest.TestCase):
    def test_each_fixture_invocation_gets_a_fresh_store(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "fixture.py"
            script.write_text(
                'import os\n'
                'from pathlib import Path\n'
                'store = Path(os.environ["NULANG_STORE_PATH"])\n'
                'marker = store / "seen"\n'
                'print("dirty" if marker.exists() else "fresh")\n'
                'marker.write_text("created")\n'
            )
            with patch.dict(os.environ, {"NULANG_STORE_PATH": str(Path(directory) / "user-store")}):
                first = run_isolated(sys.executable, str(script))
                second = run_isolated(sys.executable, str(script))
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual(first.stdout.strip(), "fresh")
            self.assertEqual(second.stdout.strip(), "fresh")
            self.assertFalse((Path(directory) / "user-store").exists())

    def test_output_and_nonzero_status_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "fixture.py"
            script.write_text('import sys\nprint("fixture output")\nsys.exit(7)\n')
            result = run_isolated(sys.executable, str(script))
            self.assertEqual(result.returncode, 7)
            self.assertEqual(result.stdout.strip(), "fixture output")


if __name__ == "__main__":
    unittest.main()
