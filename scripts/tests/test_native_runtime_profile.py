"""Tests for standalone executable startup/resource profiling."""

import importlib.util
import math
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

PROFILE = Path(__file__).resolve().parents[1] / "measure_native_runtime.py"


def load_profile():
    spec = importlib.util.spec_from_file_location("measure_native_runtime", PROFILE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@unittest.skipUnless(sys.platform == "linux" and hasattr(os, "wait4"), "requires Linux wait4")
class NativeRuntimeProfileTests(unittest.TestCase):
    def test_profile_exit_zero_samples_counts_and_nonnegative_resources(self):
        profiler = load_profile()
        result = profiler.profile_executable(Path("/bin/true"), runs=5, warmup=2)
        self.assertEqual(result["runs"], 5)
        self.assertEqual(result["warmup"], 2)
        for metric in ("median_wall_ns", "min_wall_ns", "mad_wall_ns", "median_cpu_ns", "median_peak_rss_kib"):
            self.assertTrue(math.isfinite(result[metric]), metric)
            self.assertGreaterEqual(result[metric], 0, metric)
        self.assertGreater(result["median_wall_ns"], 0)
        self.assertGreaterEqual(result["median_wall_ns"], result["min_wall_ns"])
        self.assertIn("fork", result["measurement_contract"])
        # Linux posix_spawn can use CLONE_VM: wait4 then reports the Python
        # parent RSS (~100 MiB) instead of the tiny /bin/true process RSS.
        self.assertLess(result["median_peak_rss_kib"], 16 * 1024)

    def test_nonzero_exit_is_a_failure(self):
        profiler = load_profile()
        with self.assertRaisesRegex(RuntimeError, "nonzero exit"):
            profiler.profile_executable(Path("/bin/false"), runs=3, warmup=0)

    def test_invalid_sampling_configuration_fails_early(self):
        profiler = load_profile()
        with self.assertRaisesRegex(ValueError, "runs"):
            profiler.profile_executable(Path("/bin/true"), runs=0)
        with self.assertRaisesRegex(ValueError, "warmup"):
            profiler.profile_executable(Path("/bin/true"), runs=1, warmup=-1)

    def test_cannot_execute_non_executable_file(self):
        profiler = load_profile()
        with tempfile.TemporaryDirectory() as tmp:
            artifact = Path(tmp) / "not-executable"
            artifact.write_bytes(b"not a binary")
            with self.assertRaisesRegex(RuntimeError, "not executable"):
                profiler.profile_executable(artifact, runs=2, warmup=0)


# Run the argument parser / report writer through a process boundary, too.
class RuntimeCliContractTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "requires Linux C toolchain")
    def test_cli_writes_named_sample_report(self):
        import json
        import subprocess
        with tempfile.TemporaryDirectory() as tmp:
            report_file = Path(tmp) / "measurements.json"
            result = subprocess.run(
                [sys.executable, str(PROFILE), "--runs", "3", "--warmup", "0",
                 "--json-out", str(report_file), "true=/bin/true"],
                text=True, capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads(report_file.read_text())
            self.assertEqual(data["schema_version"], 1)
            self.assertEqual(len(data["artifacts"]), 1)
            self.assertEqual(data["artifacts"][0]["name"], "true")
            self.assertEqual(data["artifacts"][0]["runs"], 3)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "requires Linux C toolchain")
    def test_duplicate_labels_fail_closed(self):
        import subprocess
        output = subprocess.run(
            [sys.executable, str(PROFILE), "--runs", "1", "true=/bin/true", "true=/bin/true"],
            text=True, capture_output=True,
        )
        self.assertNotEqual(output.returncode, 0)
        self.assertIn("unique NAME=EXECUTABLE", output.stderr)


if __name__ == "__main__":
    unittest.main()
