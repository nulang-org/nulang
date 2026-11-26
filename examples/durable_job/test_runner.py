#!/usr/bin/env python3
"""Harness self-tests; fake interpreter must never count as a Nulang proof."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import run_demo

HERE = Path(__file__).resolve().parent


class CrashHarnessTests(unittest.TestCase):
    def test_sqlite_comparator_recovers_and_deduplicates(self):
        result = subprocess.run([sys.executable, str(HERE / "run_demo.py"), "--baseline-only", "--timeout", "5"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS sqlite:", result.stdout)
        self.assertIn("RECOVERED=2:1, COMPLETED=2:1", result.stdout)

    def test_actor_demo_uses_bytecode_store_without_unsupported_os_fs_effects(self):
        # The actor-backed VM cannot execute Env.get, System.arg, or FS.read.
        with mock.patch.object(run_demo, "run_phase") as phase:
            run_demo.exercise("nulang", ["/some/nulang"], 5)
        self.assertEqual(phase.call_count, 3)
        for call in phase.call_args_list:
            argv = call.args[0]
            self.assertEqual(argv[:3], ["/some/nulang", "--backend", "bytecode"])
            self.assertNotIn("--with", argv)
            self.assertIn("--store", argv)
            self.assertTrue(argv[-1].endswith("job.nula"))

    def test_prepare_crash_reports_child_exit_and_stderr(self):
        with tempfile.TemporaryDirectory() as temp:
            child = Path(temp) / "bad-child.py"
            child.write_text("import sys\nprint('actual runtime failure', file=sys.stderr)\nsys.exit(5)\n")
            with self.assertRaisesRegex(RuntimeError, "actual runtime failure"):
                run_demo.run_phase([sys.executable, str(child)], Path(temp),
                                   "prepare", ["PREPARED=1:0"], 5)

    def test_runner_rejects_fake_nulang_that_loses_committed_state(self):
        with tempfile.TemporaryDirectory() as temp:
            fake = Path(temp) / "broken-nulang"
            fake.write_text(f"#!{sys.executable}\n"
                            "import os\n"
                            "import sys\n"
                            "if os.environ['DEMO_PHASE']=='prepare':\n"
                            "    print('PREPARED=1:0',flush=True)\n"
                            "    sys.stdin.readline()\n"
                            "else:\n"
                            "    print('RECOVERED=0:0',flush=True)\n"
                            "    print('COMPLETED=2:1',flush=True)\n")
            fake.chmod(0o755)
            result = subprocess.run([sys.executable, str(HERE / "run_demo.py"),
                                     "--nulang-bin", str(fake), "--timeout", "5"],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("expected ['RECOVERED=1:0']", result.stderr)
            self.assertNotIn("PASS nulang:", result.stdout)


if __name__ == "__main__":
    unittest.main()
