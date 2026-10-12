import importlib.util
import json
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "check_bench_regression.py"

spec = importlib.util.spec_from_file_location("check_bench_regression", SCRIPT)
check_bench_regression = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_bench_regression)


class RegressionManifestTests(unittest.TestCase):
    def test_manifest_preserves_threshold_and_current_delta(self):
        regressions = [
            ("scheduler/foo/1000", 100.0, 140.0, 0.40, 0.20),
            ("actor/bar", 50.0, 61.0, 0.22, 0.20),
        ]

        manifest = check_bench_regression.regression_manifest(regressions)

        self.assertEqual(1, manifest["schema"])
        self.assertEqual(
            {
                "benchmark": "scheduler/foo/1000",
                "historical_median_ns": 100.0,
                "current_ns": 140.0,
                "historical_delta": 0.40,
                "threshold": 0.20,
            },
            manifest["regressions"][0],
        )

    def test_write_manifest_creates_machine_readable_json(self):
        regressions = [("work", 10.0, 13.0, 0.30, 0.20)]
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / "regressions.json"
            check_bench_regression.write_regression_manifest(path, regressions)
            payload = json.loads(path.read_text())

        self.assertEqual("work", payload["regressions"][0]["benchmark"])
        self.assertEqual(0.20, payload["regressions"][0]["threshold"])


if __name__ == "__main__":
    unittest.main()
