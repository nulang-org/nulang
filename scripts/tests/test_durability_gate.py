"""Keep the source-compiled, disk-backed recovery regression in the durability gate."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]


class DurableReleaseGateTests(unittest.TestCase):
    def test_sqlite_compiled_reopen_is_an_explicit_release_gate(self):
        gate = (ROOT / "scripts" / "test-durability-guarantees.sh").read_text()
        required = (
            "cargo test --locked --no-default-features --features sqlite "
            "--test durable_compiled_store_restart"
        )
        self.assertIn(required + "\n", gate)
        self.assertIn("set -euo pipefail", gate)

    def test_exact_head_ci_runs_real_recovery_gate(self):
        workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text()
        self.assertIn("  durability-recovery:\n", workflow)
        self.assertIn("    name: Durability & Replay Certification\n", workflow)
        self.assertIn(
            "run: bash scripts/test-durability-guarantees.sh",
            workflow,
        )
        self.assertIn(
            "run: env NULANG_DST_SEEDS=200 cargo test --locked --lib dst_",
            workflow,
        )

if __name__ == "__main__":
    unittest.main()
