"""Subprocess-contract tests for scripts/difffuzz.sh (no Rust build required)."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "difffuzz.sh"


class DifferentialFuzzRunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "scripts").mkdir()
        (self.root / "scripts" / "difffuzz.sh").write_bytes(SCRIPT.read_bytes())
        (self.root / "scripts" / "difffuzz.sh").chmod(0o755)
        (self.root / "bin").mkdir()
        (self.root / "target" / "debug").mkdir(parents=True)
        self.trace = self.root / "invocations.txt"

        cargo = self.root / "bin" / "cargo"
        cargo.write_text("#!/usr/bin/env bash\nexit 0\n")
        cargo.chmod(0o755)

        fuzzer = self.root / "target" / "debug" / "nula_difffuzz"
        fuzzer.write_text("""#!/usr/bin/env bash
set -euo pipefail
while (( $# )); do
  case "$1" in
    --seeds) seeds="$2"; shift 2 ;;
    --seed-base) base="$2"; shift 2 ;;
    --time) time="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s,%s,%s\\n' "$base" "$seeds" "${time:-}" >> "$DIFF_FUZZ_TRACE"
if [[ "${FAIL_AT_BASE:-}" == "$base" ]]; then exit 1; fi
if [[ -n "${FUZZ_SLEEP:-}" ]]; then sleep "$FUZZ_SLEEP"; fi
""")
        fuzzer.chmod(0o755)

    def run_fuzz(self, *args, **extra_env):
        env = os.environ.copy()
        env.update({
            "PATH": f"{self.root / 'bin'}:{env['PATH']}",
            "CARGO_TARGET_DIR": str(self.root / "target"),
            "DIFF_FUZZ_TRACE": str(self.trace),
            **extra_env,
        })
        return subprocess.run(
            ["bash", "scripts/difffuzz.sh", *args],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=15,
        )

    def invocations(self):
        return self.trace.read_text().splitlines() if self.trace.exists() else []

    def test_splits_seed_range_into_bounded_processes_without_gaps(self):
        result = self.run_fuzz("--seeds", "1801", "--seed-base", "50")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations(), ["50,800,", "850,800,", "1650,201,"])

    def test_hex_seed_base_and_custom_batch_size(self):
        result = self.run_fuzz("--seeds", "5", "--seed-base", "0x10", "--batch-size", "2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations(), ["16,2,", "18,2,", "20,1,"])

    def test_failure_is_not_masked_and_stops_later_batches(self):
        result = self.run_fuzz("--seeds", "1800", FAIL_AT_BASE="800")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(self.invocations(), ["0,800,", "800,800,"])

    def test_rejects_invalid_batch_size_before_running(self):
        result = self.run_fuzz("--seeds", "900", "--batch-size", "0")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.invocations(), [])

    def test_release_build_invokes_release_binary(self):
        release = self.root / "target" / "release"
        release.mkdir()
        binary = self.root / "target" / "debug" / "nula_difffuzz"
        (release / "nula_difffuzz").write_bytes(binary.read_bytes())
        (release / "nula_difffuzz").chmod(0o755)
        result = self.run_fuzz("--release", "--seeds", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations(), ["0,1,"])

    def test_wraps_u64_seed_base_without_skipping_or_repeating(self):
        result = self.run_fuzz(
            "--seeds", "3", "--seed-base", "18446744073709551614", "--batch-size", "1"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations(), [
            "18446744073709551614,1,", "18446744073709551615,1,", "0,1,"
        ])

    def test_rejects_arithmetic_injection_or_out_of_range_values(self):
        for args in (
            ("--seed-base", "1+2"),
            ("--seed-base", "18446744073709551616"),
            ("--seeds", "999999999999999999999999"),
            ("--batch-size", "-1"),
            ("--time", "-1"),
            ("--batch-size",),
        ):
            with self.subTest(args=args):
                result = self.run_fuzz(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(self.invocations(), [])

    def test_single_global_time_limit_applies_across_batched_processes(self):
        result = self.run_fuzz("--seeds", "1800", "--time", "1", FUZZ_SLEEP="1.1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations(), ["0,800,1"])


if __name__ == "__main__":
    unittest.main()
