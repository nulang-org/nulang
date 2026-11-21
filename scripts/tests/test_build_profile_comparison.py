"""Contract tests for the optional Cargo development-profile benchmark."""

import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "compare_build_profiles.py"


def load_script():
    spec = importlib.util.spec_from_file_location("compare_build_profiles", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BuildProfileComparisonTests(unittest.TestCase):
    def test_both_profiles_use_same_feature_set_and_isolated_targets(self):
        benchmark = load_script()
        builds = []

        def fake_build(command, cwd, env, check):
            self.assertTrue(check)
            builds.append((command, cwd, env["CARGO_TARGET_DIR"]))

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(
            benchmark.subprocess, "run", side_effect=fake_build
        ), mock.patch.object(benchmark.time, "perf_counter", side_effect=range(8)):
            report = benchmark.measure(ROOT, pathlib.Path(tmp))

        self.assertEqual(len(builds), 4)
        commands = [item[0] for item in builds]
        for cmd in commands:
            self.assertEqual(cmd[:7], ["cargo", "build", "--locked", "--no-default-features", "--bin", "nulang", "--profile"])
            self.assertIn("--timings", cmd)
        self.assertEqual([c[7] for c in commands], ["dev", "dev", "dev-fast", "dev-fast"])
        self.assertEqual(builds[0][2], builds[1][2])
        self.assertEqual(builds[2][2], builds[3][2])
        self.assertNotEqual(builds[0][2], builds[2][2])
        self.assertEqual(report["dev"]["cold_seconds"], 1)
        self.assertEqual(report["dev-fast"]["warm_seconds"], 1)

    def test_failed_build_raises_without_reporting_a_faster_result(self):
        benchmark = load_script()
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(
            benchmark.subprocess, "run", side_effect=RuntimeError("cargo failed")
        ):
            with self.assertRaisesRegex(RuntimeError, "cargo failed"):
                benchmark.measure(ROOT, pathlib.Path(tmp))

    def test_output_is_machine_readable_and_parent_is_created(self):
        benchmark = load_script()
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "new" / "measurements.json"
            benchmark.write_report(path, {"dev": {"cold_seconds": 3.5}})
            self.assertEqual(json.loads(path.read_text())["dev"]["cold_seconds"], 3.5)


if __name__ == "__main__":
    unittest.main()
