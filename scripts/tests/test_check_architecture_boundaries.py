import importlib.util
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "check_architecture_boundaries.py"

spec = importlib.util.spec_from_file_location("check_architecture_boundaries", SCRIPT)
check_architecture_boundaries = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_architecture_boundaries)


class CloudControlBoundaryTests(unittest.TestCase):
    def _write_repo(self, root_manifest: str, member_manifests=None):
        tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(tmp.name)
        (root / "Cargo.toml").write_text(root_manifest, encoding="utf-8")
        for relpath, content in (member_manifests or {}).items():
            path = root / relpath
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        return tmp, root

    def test_allows_frozen_migration_crate_outside_active_workspace(self):
        tmp, root = self._write_repo(
            """
[workspace]
members = ["crates/nulang-capacity"]

[package]
name = "nulang"
version = "0.1.0"

[dependencies]
serde = "1"
""",
            {
                "crates/nulang-capacity/Cargo.toml": """
[package]
name = "nulang-capacity"
version = "0.1.0"
""",
                "crates/nulang-cloud-control/Cargo.toml": """
[package]
name = "nulang-cloud-control"
version = "0.1.0"
""",
            },
        )
        self.addCleanup(tmp.cleanup)

        self.assertEqual([], check_architecture_boundaries.validate_repository(root))

    def test_rejects_cloud_control_as_workspace_member(self):
        tmp, root = self._write_repo(
            """
[workspace]
members = ["crates/nulang-capacity", "crates/nulang-cloud-control"]

[package]
name = "nulang"
version = "0.1.0"
""",
            {
                "crates/nulang-capacity/Cargo.toml": """
[package]
name = "nulang-capacity"
version = "0.1.0"
""",
                "crates/nulang-cloud-control/Cargo.toml": """
[package]
name = "nulang-cloud-control"
version = "0.1.0"
""",
            },
        )
        self.addCleanup(tmp.cleanup)

        errors = check_architecture_boundaries.validate_repository(root)

        self.assertTrue(any("must not be an active workspace member" in error for error in errors))

    def test_rejects_dependency_on_cloud_control_from_active_member(self):
        tmp, root = self._write_repo(
            """
[workspace]
members = ["crates/nulang-capacity"]

[package]
name = "nulang"
version = "0.1.0"
""",
            {
                "crates/nulang-capacity/Cargo.toml": """
[package]
name = "nulang-capacity"
version = "0.1.0"

[dependencies]
nulang-cloud-control = { path = "../nulang-cloud-control" }
""",
            },
        )
        self.addCleanup(tmp.cleanup)

        errors = check_architecture_boundaries.validate_repository(root)

        self.assertTrue(any("depends on forbidden package nulang-cloud-control" in error for error in errors))

    def test_rejects_renamed_target_specific_dependency(self):
        tmp, root = self._write_repo(
            """
[workspace]
members = ["crates/nulang-capacity"]

[package]
name = "nulang"
version = "0.1.0"
""",
            {
                "crates/nulang-capacity/Cargo.toml": """
[package]
name = "nulang-capacity"
version = "0.1.0"

[target.'cfg(unix)'.dependencies]
legacy-placement = { package = "nulang-cloud-control", path = "../nulang-cloud-control" }
""",
            },
        )
        self.addCleanup(tmp.cleanup)

        errors = check_architecture_boundaries.validate_repository(root)

        self.assertTrue(any("depends on forbidden package nulang-cloud-control" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
