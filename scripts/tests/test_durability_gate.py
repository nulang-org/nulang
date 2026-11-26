"""Keep disk-backed and OS-process recovery regressions in the durability gate."""

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

    def test_sqlite_process_kill_is_an_explicit_release_gate(self):
        gate = (ROOT / "scripts" / "test-durability-guarantees.sh").read_text()
        required = (
            "cargo test --locked --no-default-features --features sqlite "
            "--test durable_effect_process_kill"
        )
        self.assertIn(required + "\n", gate)
        self.assertIn("set -euo pipefail", gate)


    def test_sqlite_vm_callback_is_an_explicit_release_gate(self):
        gate = (ROOT / "scripts" / "test-durability-guarantees.sh").read_text()
        required = (
            "cargo test --locked --no-default-features --features sqlite "
            "--test durable_vm_callback_bridge"
        )
        self.assertIn(required + "\n", gate)
        self.assertIn("set -euo pipefail", gate)

    def test_sqlite_workflow_occurrence_binding_is_an_explicit_release_gate(self):
        gate = (ROOT / "scripts" / "test-durability-guarantees.sh").read_text()
        required = (
            "cargo test --locked --no-default-features --features sqlite "
            "--test durable_effect_recovery_store"
        )
        self.assertIn(required + "\n", gate)
        self.assertIn("set -euo pipefail", gate)


    def test_workflow_command_admission_and_recovery_are_explicit_release_gates(self):
        gate = (ROOT / "scripts" / "test-durability-guarantees.sh").read_text()
        self.assertIn(
            "cargo test --locked --no-default-features --features sqlite --test workflow_effect_tail_recovery\n",
            gate,
        )
        self.assertIn(
            "cargo test --locked --no-default-features --lib test_recovery_replays_unfinished_atomic_native_workflow_activation\n",
            gate,
        )


if __name__ == "__main__":
    unittest.main()
