"""Exercise the disk-backed Cargo wrapper without compiling Rust."""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts/cargo-disk.sh"


class CargoDiskWrapperTest(unittest.TestCase):
    def fake_cargo(self, bin_dir, body):
        executable = Path(bin_dir) / "cargo"
        executable.write_text("#!/bin/sh\n" + body)
        executable.chmod(0o755)

    def test_forwards_arguments_with_repo_local_compiler_tmpdir(self):
        with tempfile.TemporaryDirectory() as bin_dir:
            self.fake_cargo(bin_dir, 'printf "%s|%s|%s\\n" "$TMPDIR" "$PWD" "$*"\n')
            env = os.environ.copy()
            env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
            result = subprocess.run(
                ["bash", str(WRAPPER), "check", "--locked"],
                cwd="/",
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
        self.assertEqual(
            result.stdout.strip(),
            f"{ROOT / '.build-tmp'}|{ROOT}|check --locked",
        )
        self.assertTrue((ROOT / ".build-tmp").is_dir())

    def test_preserves_cargo_failure(self):
        with tempfile.TemporaryDirectory() as bin_dir:
            self.fake_cargo(bin_dir, "exit 42\n")
            env = os.environ.copy()
            env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
            result = subprocess.run(
                ["bash", str(WRAPPER), "check"],
                env=env,
                capture_output=True,
                text=True,
            )
        self.assertEqual(result.returncode, 42)


if __name__ == "__main__":
    unittest.main()
