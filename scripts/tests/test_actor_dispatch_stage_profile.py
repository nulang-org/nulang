"""Contracts for actor dispatch stage profiles and honest JIT labels."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DispatchStageProfileContracts(unittest.TestCase):
    def test_native_noop_drain_is_separate_from_mailbox_and_enqueue(self):
        source = (ROOT / "src/benchmarks.rs").read_text()
        self.assertIn("fn bench_ab_native_noop_drain()", source)
        self.assertIn('report_ab("native_noop_drain"', source)
        self.assertIn('report_ab("mailbox_push_inline_1"', source)
        self.assertIn('report_ab(&format!("enqueue_payload_{arity}"', source)

    def test_jit_enabled_bytecode_drain_exposes_actual_tier_compilations(self):
        source = (ROOT / "src/benchmarks.rs").read_text()
        runtime = (ROOT / "src/runtime/mod.rs").read_text()
        self.assertIn("benchmark_jit_compiled_count()", source)
        self.assertIn("pub(crate) fn benchmark_jit_compiled_count(&self)", runtime)
        self.assertIn("jit_compiled_regions", source)
        self.assertIn("jit_execution_verified", source)
        self.assertNotIn("warmed bytecode/JIT actor must process every message", source)

    def test_ci_executes_stage_profile_contracts(self):
        workflow = (ROOT / ".github/workflows/nulang-ab-bench.yml").read_text()
        self.assertIn("test_actor_dispatch_stage_profile.py", workflow)


if __name__ == "__main__":
    unittest.main()
