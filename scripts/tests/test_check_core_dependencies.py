"""Contract tests for the minimal-core Cargo dependency graph guard."""

import pathlib
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import check_core_dependencies as guard  # noqa: E402


def graph(*edges):
    """Synthetic Cargo metadata; each edge is (parent, child, kind)."""
    all_names = {"nulang"}
    all_names.update(x for parent, child, _ in edges for x in (parent, child))
    nodes = []
    for name in sorted(all_names):
        nodes.append({
            "id": name,
            "deps": [
                {"pkg": child, "dep_kinds": [{"kind": kind, "target": None}]}
                for parent, child, kind in edges if parent == name
            ],
        })
    return {
        "packages": [{"id": name, "name": name} for name in sorted(all_names)],
        "resolve": {"nodes": nodes},
    }


class CoreDependencyBoundaryTests(unittest.TestCase):
    def test_minimal_core_accepts_ordinary_runtime_dependencies(self):
        reached, violations = guard.inspect_core(graph(
            ("nulang", "serde", None),
            ("serde", "proc-macro2", "build"),
        ))
        self.assertEqual(reached, ["nulang", "proc-macro2", "serde"])
        self.assertEqual(violations, [])

    def test_transitive_optional_runtime_is_rejected_with_explanation(self):
        _, violations = guard.inspect_core(graph(
            ("nulang", "adapter", None),
            ("adapter", "tokio", None),
        ))
        self.assertEqual(violations, ["nulang -> adapter -> tokio"])

    def test_compiler_backend_family_is_forbidden(self):
        _, violations = guard.inspect_core(graph(
            ("nulang", "cranelift-codegen", None),
            ("nulang", "nulang-ai-core", None),
        ))
        self.assertEqual(violations, [
            "nulang -> cranelift-codegen", "nulang -> nulang-ai-core",
        ])

    def test_dev_only_and_unreachable_workspace_packages_are_excluded(self):
        reached, violations = guard.inspect_core(graph(
            ("nulang", "serde", None),
            ("nulang", "tokio", "dev"),
            ("another-workspace-crate", "wasmtime", None),
        ))
        self.assertEqual(reached, ["nulang", "serde"])
        self.assertEqual(violations, [])

    def test_build_dependencies_are_checked(self):
        _, violations = guard.inspect_core(graph(("nulang", "pyo3", "build")))
        self.assertEqual(violations, ["nulang -> pyo3"])

    def test_malformed_graph_fails_closed(self):
        metadata = graph(("nulang", "serde", None))
        metadata["resolve"]["nodes"] = [
            node for node in metadata["resolve"]["nodes"]
            if node["id"] != "serde"
        ]
        with self.assertRaisesRegex(ValueError, "Missing resolved dependency"):
            guard.inspect_core(metadata)

    def test_metadata_command_limits_feature_resolution_to_root(self):
        with patch.object(guard.subprocess, "run") as run:
            run.return_value.stdout = '{"packages":[],"resolve":{"nodes":[]}}'
            guard.load_metadata(pathlib.Path("/test-repo"))
        command = run.call_args.args[0]
        self.assertIn("--no-default-features", command)
        self.assertNotIn("--package", command)
        self.assertIn("--format-version", command)
        self.assertIn("--locked", command)


if __name__ == "__main__":
    unittest.main()
