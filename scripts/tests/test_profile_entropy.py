"""Tests for resolved per-profile package and build artifact measurements."""

import pathlib
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import profile_entropy as m


def graph():
    packages = [
        {"id": "root", "name": "nulang", "version": "0.1.0"},
        {"id": "serde", "name": "serde", "version": "1.0"},
        {"id": "shared", "name": "shared", "version": "1.0"},
        {"id": "dev", "name": "criterion", "version": "0.5"},
        {"id": "other", "name": "other-workspace", "version": "1.0"},
        {"id": "wasmtime", "name": "wasmtime", "version": "49"},
    ]
    return {"packages": packages, "resolve": {"nodes": [
        {"id": "root", "deps": [
            {"pkg": "serde", "dep_kinds": [{"kind": None}]},
            {"pkg": "shared", "dep_kinds": [{"kind": "build"}]},
            {"pkg": "dev", "dep_kinds": [{"kind": "dev"}]},
        ]},
        {"id": "serde", "deps": []}, {"id": "shared", "deps": []},
        {"id": "dev", "deps": []},
        {"id": "other", "deps": [{"pkg": "wasmtime", "dep_kinds": [{"kind": None}]}]},
        {"id": "wasmtime", "deps": []},
    ]}}


class ProfileEntropyTests(unittest.TestCase):
    def test_profile_flags_are_explicit_and_not_default_implicit(self):
        self.assertEqual(m.PROFILES["minimal"], ())
        self.assertEqual(m.PROFILES["release-cli"], ("release-cli",))
        self.assertEqual(m.PROFILES["full"], ("full",))

    def test_counts_transitive_build_dependencies_not_dev_or_workspace(self):
        report = m.summarize(graph())
        self.assertEqual(report["resolved_packages"], 3)
        self.assertEqual(report["direct_packages"], 2)
        self.assertEqual(report["package_names"], ["nulang", "serde", "shared"])

    def test_missing_resolved_node_fails_closed(self):
        data = graph()
        data["resolve"]["nodes"] = [n for n in data["resolve"]["nodes"] if n["id"] != "serde"]
        with self.assertRaisesRegex(ValueError, "Incomplete resolved graph"):
            m.summarize(data)

    def test_missing_package_identity_fails_closed(self):
        data = graph()
        data["packages"] = [p for p in data["packages"] if p["id"] != "serde"]
        with self.assertRaises(ValueError):
            m.summarize(data)

    def test_metadata_command_is_feature_specific_and_platform_filtered(self):
        with patch.object(m.subprocess, "run") as mock:
            mock.return_value.stdout = '{"packages":[],"resolve":{"nodes":[]}}'
            m.metadata("/repo", "release-cli", "x86_64-unknown-linux-gnu")
        argv = mock.call_args.args[0]
        self.assertIn("--locked", argv)
        self.assertIn("--no-default-features", argv)
        self.assertEqual(argv[argv.index("--features") + 1], "release-cli")
        self.assertEqual(argv[argv.index("--filter-platform") + 1], "x86_64-unknown-linux-gnu")
        self.assertNotIn("--package", argv)

    def test_missing_binary_is_reported_unknown_not_zero(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertIsNone(m.binary_size(pathlib.Path(d) / "missing"))

    def test_binary_size_measured_from_existing_artifact(self):
        with tempfile.TemporaryDirectory() as d:
            path = pathlib.Path(d) / "nulang"
            path.write_bytes(b"abcd")
            self.assertEqual(m.binary_size(path), 4)

    def test_invalid_root_and_missing_dep_kinds_fail_closed(self):
        data = graph()
        data["resolve"]["nodes"][0]["deps"][0].pop("dep_kinds")
        with self.assertRaises(ValueError):
            m.summarize(data)

    def test_error_exit_from_metadata_is_not_downgraded_to_empty_report(self):
        import subprocess
        with patch.object(m.subprocess, "run", side_effect=subprocess.CalledProcessError(101, ["cargo"])):
            with self.assertRaises(subprocess.CalledProcessError):
                m.metadata("/repo", "minimal", "x86_64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
