import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
VM_RS = ROOT / "src" / "vm.rs"


class ColdJitProbeGateTests(unittest.TestCase):
    def test_step_checks_candidate_before_try_jit_execute_call(self):
        source = VM_RS.read_text(encoding="utf-8")
        step_start = source.index("    pub fn step(&mut self) -> NuResult<()> {")
        dispatch_start = source.index("        match instr.opcode {", step_start)
        step_prefix = source[step_start:dispatch_start]

        candidate_gate = "self.jit_candidate_for_frame(frame_idx)"
        jit_call = "self.try_jit_execute(frame_idx)"

        self.assertIn(
            candidate_gate,
            step_prefix,
            "cold interpreter steps must reject non-candidate PCs before calling the large JIT entry path",
        )
        self.assertLess(
            step_prefix.index(candidate_gate),
            step_prefix.index(jit_call),
            "candidate gate must execute before try_jit_execute",
        )

    def test_try_jit_execute_does_not_repeat_candidate_bitmap_lookup(self):
        source = VM_RS.read_text(encoding="utf-8")
        start = source.index("    fn try_jit_execute(&mut self, frame_idx: usize) -> bool {")
        end = source.index("\n    #[cfg(not(feature = \"native-codegen\"))]", start)
        body = source[start:end]

        self.assertNotIn(
            ".jit_candidate_pcs",
            body,
            "candidate classification belongs in step() so non-candidate instructions avoid the function call entirely",
        )

    def test_minimal_build_has_false_candidate_fallback(self):
        source = VM_RS.read_text(encoding="utf-8")
        fallback = (
            '#[cfg(not(feature = "native-codegen"))]\n'
            '    #[inline(always)]\n'
            '    fn jit_candidate_for_frame(&self, _frame_idx: usize) -> bool {\n'
            '        false\n'
            '    }'
        )
        self.assertIn(
            fallback,
            source,
            "no-default-features builds need a candidate helper that always rejects JIT entry",
        )


if __name__ == "__main__":
    unittest.main()
