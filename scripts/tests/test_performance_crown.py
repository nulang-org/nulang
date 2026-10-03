import pathlib
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import performance_crown


class PerformanceCrownTests(unittest.TestCase):
    def make_manifest(self):
        return {
            "schema": 1,
            "domains": [
                {
                    "id": domain,
                    "benchmarks": [
                        {
                            "id": f"{domain}.probe",
                            "status": "planned" if domain != "concurrent" else "comparative",
                            "metric": "latency_ns",
                            "runner": None if domain != "concurrent" else "scripts/runner.py",
                            "baselines": [] if domain != "concurrent" else ["rust", "go"],
                            "measurement_mode": "planned" if domain != "concurrent" else "controlled",
                        }
                    ],
                }
                for domain in performance_crown.REQUIRED_DOMAINS
            ],
        }

    def test_valid_manifest_and_claim_ready_detection(self):
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            (root / "scripts").mkdir()
            (root / "scripts" / "runner.py").write_text("# runner\n")
            manifest = self.make_manifest()
            self.assertEqual([], performance_crown.validate_manifest(manifest, root))
            rows = performance_crown.rows(manifest)
            ready = [row["id"] for row in rows if row["claim_ready"]]
            self.assertEqual(["concurrent.probe"], ready)

    def test_missing_required_domain_is_rejected(self):
        manifest = self.make_manifest()
        manifest["domains"] = manifest["domains"][:-1]
        errors = performance_crown.validate_manifest(manifest)
        self.assertTrue(any("missing required domains" in error for error in errors))

    def test_comparative_benchmark_requires_controlled_mode_and_baseline(self):
        manifest = self.make_manifest()
        bench = manifest["domains"][1]["benchmarks"][0]
        bench["baselines"] = []
        bench["measurement_mode"] = "host"
        errors = performance_crown.validate_manifest(manifest)
        self.assertTrue(any("requires at least one baseline" in error for error in errors))
        self.assertTrue(any("requires measurement_mode=controlled" in error for error in errors))

    def test_existing_runner_must_resolve_inside_repo(self):
        manifest = self.make_manifest()
        errors = performance_crown.validate_manifest(manifest, pathlib.Path("/definitely/missing"))
        self.assertTrue(any("runner does not exist" in error for error in errors))

    def test_repository_manifest_is_valid(self):
        manifest = performance_crown.load_manifest(
            ROOT / "benchmarks" / "performance_crown.json"
        )
        self.assertEqual([], performance_crown.validate_manifest(manifest, ROOT))
        self.assertEqual(
            set(performance_crown.REQUIRED_DOMAINS),
            {row["domain"] for row in performance_crown.rows(manifest)},
        )


if __name__ == "__main__":
    unittest.main()
