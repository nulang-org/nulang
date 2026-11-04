"""CI regression: the WASM Rust matrix must fit standard runner disk budgets.

The WASM lane builds *all targets*, including large Wasmtime test binaries.
On 2026-11-04 its hosted runner failed to link with ENOSPC. Keep disk-saving
Cargo settings scoped to this lane and retain the executable test gate.
"""

import pathlib
import re
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


def wasm_job() -> str:
    text = WORKFLOW.read_text(encoding="utf-8")
    match = re.search(
        r"(?ms)^  wasm-backend:\n(.*?)(?=^  [a-z][a-z0-9-]*:\n|\Z)",
        text,
    )
    if match is None:
        raise AssertionError("wasm-backend CI job missing")
    return match.group(1)


class WasmCiDiskBudgetContract(unittest.TestCase):
    def test_wasm_job_uses_bounded_artifact_profile(self):
        job = wasm_job()
        for setting in (
            "CARGO_PROFILE_DEV_DEBUG: '0'",
            "CARGO_PROFILE_TEST_DEBUG: '0'",
            "CARGO_INCREMENTAL: '0'",
            "CARGO_BUILD_JOBS: '2'",
        ):
            with self.subTest(setting=setting):
                self.assertIn(setting, job)

    def test_wasm_job_keeps_executable_build_and_test_gates(self):
        job = wasm_job()
        self.assertIn("cargo build --locked --all-targets --features wasm-backend", job)
        self.assertIn("cargo test --locked --features wasm-backend", job)

    def test_security_audit_gate_remains_blocking(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("cargo audit", text)
        self.assertNotIn("cargo audit || true", text)


if __name__ == "__main__":
    unittest.main()
