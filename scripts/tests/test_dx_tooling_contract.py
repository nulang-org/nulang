"""Keep Dev Plane's DX commands aligned with the contributor scripts."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DevPlaneDeveloperExperienceTests(unittest.TestCase):
    def test_doctor_uses_full_portable_environment_validation(self):
        config = (ROOT / 'devplane.yaml').read_text()
        self.assertIn('  doctor: bash scripts/dev-doctor.sh', config)
        self.assertNotIn('doctor: cargo --version && rustc --version', config)

    def test_first_run_gate_builds_before_running_smoke(self):
        config = (ROOT / 'devplane.yaml').read_text()
        self.assertIn('  first_run:\n', config)
        self.assertIn('cargo build --locked --bin nulang && python3 scripts/smoke_first_run.py --bin target/debug/nulang', config)


if __name__ == '__main__':
    unittest.main()
