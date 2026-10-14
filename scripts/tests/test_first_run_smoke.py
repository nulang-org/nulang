"""Contract tests for the isolated Nulang first-run smoke driver."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "smoke_first_run.py"

FAKE = '''#!/usr/bin/env python3
import os
from pathlib import Path
import sys
args = sys.argv[1:]
with open(os.environ["SMOKE_LOG"], "a") as log:
    log.write(" ".join(args) + "\\n")
if " ".join(args) == os.environ.get("SMOKE_FAIL", "<never>"):
    print("synthetic failure", file=sys.stderr)
    sys.exit(9)
if args == ["--version"]:
    print("nulang 0.1.0")
elif args == ["--eval", 'perform IO.print("first-run-eval-ok")']:
    print("first-run-eval-ok")
elif args == ["nula", "new", "smoke-app", "--template", "lib"]:
    root = Path.cwd() / "smoke-app"
    (root / "src").mkdir(parents=True)
    (root / "tests").mkdir()
    (root / "Nulang.toml").write_text('[package]\\nname = "smoke-app"\\n')
    (root / "src" / "main.nula").write_text('fn main() {}\\n')
    (root / "tests" / "test_add.nula").write_text('fn main() {}\\n')
elif args == ["--check", "src/main.nula"]:
    assert (Path.cwd() / "src" / "main.nula").is_file()
elif args == ["nula", "build"]:
    target = Path.cwd() / ".nula" / "dist"
    target.mkdir(parents=True)
    (target / "smoke-app.nbc").write_bytes(b"test")
elif args == ["nula", "run"]:
    print("Library package ready.")
elif args == ["nula", "test"]:
    print(os.environ.get("SMOKE_TEST_RESULT", "test result: 2 passed; 0 failed"))
elif args == ["fmt", "stripped.nula"]:
    (Path.cwd() / "stripped.nula").write_text("type SmokeColor = Red | Green\\n")
elif args == ["fmt", "--check", "stripped.nula"]:
    assert (Path.cwd() / "stripped.nula").read_text() == "type SmokeColor = Red | Green\\n"
else:
    sys.exit("unrecognized command: " + str(args))
'''

class FirstRunSmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.fake = root / "nulang"
        self.fake.write_text(FAKE)
        self.fake.chmod(0o755)
        self.log = root / "calls.log"

    def invoke(self, **extra):
        env = os.environ.copy()
        env["SMOKE_LOG"] = str(self.log)
        env.update(extra)
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--bin", str(self.fake)],
            cwd=self.temp.name, env=env, capture_output=True, text=True,
        )

    def test_happy_path_covers_new_check_build_run_test_and_formatter(self):
        proc = self.invoke()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = self.log.read_text().splitlines()
        self.assertEqual(calls, [
            "--version",
            '--eval perform IO.print("first-run-eval-ok")',
            "nula new smoke-app --template lib",
            "--check src/main.nula", "nula build", "nula run", "nula test",
            "fmt stripped.nula", "fmt --check stripped.nula",
        ])
        self.assertIn("first-run smoke passed", proc.stdout)

    def test_failure_is_reported_and_stops_before_later_stages(self):
        proc = self.invoke(SMOKE_FAIL="nula test")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("nula test", proc.stderr)
        self.assertIn("synthetic failure", proc.stderr)
        self.assertNotIn("fmt stripped.nula", self.log.read_text())

    def test_zero_discovered_tests_is_not_a_success(self):
        proc = self.invoke(SMOKE_TEST_RESULT="test result: 0 passed; 0 failed")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("nula test", proc.stderr)

    def test_failed_test_summary_is_not_a_success_even_with_zero_exit(self):
        proc = self.invoke(SMOKE_TEST_RESULT="test result: 1 passed; 1 failed")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("nula test", proc.stderr)

    def test_missing_binary_fails_before_any_command(self):
        self.fake.unlink()
        proc = self.invoke()
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("binary not found", proc.stderr)
        self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
