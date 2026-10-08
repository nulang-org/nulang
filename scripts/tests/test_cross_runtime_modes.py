"""Cross-runtime Nulang AOT backend contract tests.

The harness must not relabel bytecode data as AOT. These tests guard
build isolation, honest output identity, and fail-closed native execution.
"""
from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import cross_runtime_bench as harness


class CommandDiagnosticsTests(unittest.TestCase):
    def test_failed_cargo_command_surfaces_captured_compiler_output(self):
        import subprocess
        from contextlib import redirect_stderr
        from io import StringIO

        simulated = subprocess.CompletedProcess(
            args=["cargo", "build"],
            returncode=101,
            stdout="error[E0308]: mismatched types in native backend\\n",
            stderr=None,
        )
        errors = StringIO()
        with mock.patch.object(harness.subprocess, "run", return_value=simulated):
            with redirect_stderr(errors):
                with self.assertRaises(subprocess.CalledProcessError):
                    harness.command_output(["cargo", "build"])
        self.assertIn("error[E0308]", errors.getvalue())


class NativeBackendBuildTests(unittest.TestCase):
    def test_bytecode_and_aot_builds_use_distinct_saved_binaries(self):
        outputs = []
        copied = []

        def run(command, **kwargs):
            outputs.append(command)
            if command[:2] == ["cargo", "metadata"]:
                return '{"target_directory": "/target/test"}'
            return ""

        def fake_copy(source, dest):
            copied.append((str(source), str(dest)))
            return str(dest)

        with (
            mock.patch.object(harness, "command_output", side_effect=run),
            mock.patch.object(harness.shutil, "which", return_value="/usr/bin/cargo"),
            mock.patch.object(harness.shutil, "copy2", side_effect=fake_copy),
        ):
            commands = harness.build_commands(["nulang", "nulang-aot"])

        cargo_builds = [command for command in outputs if command[:2] == ["cargo", "build"]]
        self.assertEqual(len(cargo_builds), 2)
        self.assertIn("savina-bench", cargo_builds[0])
        self.assertNotIn("native-codegen", " ".join(cargo_builds[0]))
        self.assertIn("native-codegen", " ".join(cargo_builds[1]))
        self.assertIn("savina-bench", " ".join(cargo_builds[1]))
        self.assertEqual(len(copied), 2)
        self.assertNotEqual(commands["nulang"][0], commands["nulang-aot"][0])
        self.assertIn("--backend", commands["nulang"])
        self.assertIn("aot", commands["nulang-aot"])
        self.assertIn("bytecode", commands["nulang"])
        self.assertIn("--cross-runtime-only", commands["nulang"])
        self.assertIn("--cross-runtime-only", commands["nulang-aot"])

    def test_aot_results_are_not_accepted_as_bytecode(self):
        output = "\n".join(
            f"[cross-bench] runtime=nulang-aot benchmark={name} messages=100 elapsed_ns=2500"
            for name in harness.BENCHMARKS
        )
        self.assertEqual(len(harness.parse_records(output, "nulang-aot")), 4)
        with self.assertRaises(RuntimeError):
            harness.parse_records(output, "nulang")


class NativeBackendSourceContractTests(unittest.TestCase):
    def test_aot_backend_compiles_mir_and_registers_before_actor_spawn(self):
        source = (ROOT / "src/bin/nulang_savina.rs").read_text()
        self.assertIn("--backend", source)
        self.assertIn("--cross-runtime-only", source)
        self.assertIn("config.cross_runtime_only", source)
        self.assertIn("AotModule::compile(&mir)", source)
        self.assertIn("register_aot_module(aot)", source)
        self.assertIn("aot_targets", source)
        self.assertIn("meta.behavior_indices.len()", source)
        self.assertNotIn("aot_targets.len() == actor.bytecode_offsets.len()", source)
        self.assertIn("native-codegen", source)
        self.assertIn('"nulang-aot"', source)

    def test_workflow_explicitly_runs_aot_comparison(self):
        workflow = (ROOT / ".github/workflows/cross-runtime-bench.yml").read_text()
        self.assertIn("nulang-aot", workflow)
        self.assertIn("test_cross_runtime*.py", workflow)


if __name__ == "__main__":
    unittest.main()
