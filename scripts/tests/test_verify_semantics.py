import importlib.util
import json
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "verify_semantics.py"

spec = importlib.util.spec_from_file_location("verify_semantics", MODULE_PATH)
verify_semantics = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify_semantics)


class VerifySemanticsTests(unittest.TestCase):
    def write_repo(self, invariants, conformance, evidence=("src/vm.rs",)):
        td = tempfile.TemporaryDirectory()
        root = pathlib.Path(td.name)
        (root / "spec/invariants").mkdir(parents=True)
        (root / "spec/backend_conformance").mkdir(parents=True)
        for path in evidence:
            p = root / path
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("// evidence\n", encoding="utf-8")
        (root / "spec/invariants/v0alpha1.json").write_text(
            json.dumps(invariants), encoding="utf-8"
        )
        (root / "spec/backend_conformance/v0alpha1.json").write_text(
            json.dumps(conformance), encoding="utf-8"
        )
        return td, root

    def valid_docs(self):
        invariants = {
            "schema_version": 1,
            "invariants": [
                {
                    "id": "SEM-INT-001",
                    "domain": "semantics",
                    "status": "partial",
                    "summary": "Integer semantics agree across enabled backends.",
                    "evidence": ["src/vm.rs"],
                }
            ],
        }
        conformance = {
            "schema_version": 1,
            "reference_backend": "bytecode",
            "backends": [
                {
                    "id": "bytecode",
                    "role": "semantic-reference",
                    "status": "primary",
                },
                {
                    "id": "native",
                    "role": "whole-module-native",
                    "status": "experimental",
                },
            ],
            "features": [
                {
                    "id": "integer-semantics",
                    "invariant": "SEM-INT-001",
                    "support": {"bytecode": "reference", "native": "partial"},
                }
            ],
        }
        return invariants, conformance

    def test_accepts_valid_registry_and_matrix(self):
        invariants, conformance = self.valid_docs()
        td, root = self.write_repo(invariants, conformance)
        self.addCleanup(td.cleanup)
        self.assertEqual([], verify_semantics.validate(root))

    def test_rejects_unknown_invariant_reference(self):
        invariants, conformance = self.valid_docs()
        conformance["features"][0]["invariant"] = "SEM-INT-999"
        td, root = self.write_repo(invariants, conformance)
        self.addCleanup(td.cleanup)
        errors = verify_semantics.validate(root)
        self.assertTrue(any("unknown invariant" in error for error in errors))

    def test_rejects_unknown_backend_in_support_map(self):
        invariants, conformance = self.valid_docs()
        conformance["features"][0]["support"]["ghost"] = "verified"
        td, root = self.write_repo(invariants, conformance)
        self.addCleanup(td.cleanup)
        errors = verify_semantics.validate(root)
        self.assertTrue(any("unknown backend" in error for error in errors))

    def test_rejects_missing_evidence_path(self):
        invariants, conformance = self.valid_docs()
        invariants["invariants"][0]["evidence"] = ["src/missing.rs::test_case"]
        td, root = self.write_repo(invariants, conformance)
        self.addCleanup(td.cleanup)
        errors = verify_semantics.validate(root)
        self.assertTrue(any("evidence path does not exist" in error for error in errors))

    def test_rejects_invalid_status(self):
        invariants, conformance = self.valid_docs()
        conformance["features"][0]["support"]["native"] = "perfect"
        td, root = self.write_repo(invariants, conformance)
        self.addCleanup(td.cleanup)
        errors = verify_semantics.validate(root)
        self.assertTrue(any("invalid support status" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
