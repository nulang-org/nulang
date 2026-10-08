"""Contracts: a warmed JIT mode is distinct from bytecode and AOT.

JIT results are valid only when the actor runtime has actually compiled a
region during an untimed warmup on the very runtime later measured.
"""
import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import cross_runtime_bench as harness


class JitBuildContract(unittest.TestCase):
    def test_three_nulang_modes_keep_binary_and_result_identity_separate(self):
        commands = []

        def fake_run(command, **_):
            commands.append(command)
            if command[:2] == ["cargo", "metadata"]:
                return '{"target_directory": "/tmp/cargo"}'
            return ""

        with (
            mock.patch.object(harness, "command_output", side_effect=fake_run),
            mock.patch.object(harness.shutil, "which", return_value="/usr/bin/cargo"),
            mock.patch.object(harness.shutil, "copy2"),
        ):
            binaries = harness.build_commands(["nulang", "nulang-jit", "nulang-aot"])
        self.assertEqual(len(binaries), 3)
        self.assertEqual(len({value[0] for value in binaries.values()}), 3)
        self.assertIn("jit", binaries["nulang-jit"])
        self.assertIn("aot", binaries["nulang-aot"])
        self.assertIn("bytecode", binaries["nulang"])
        self.assertEqual(
            len([cmd for cmd in commands if cmd[:2] == ["cargo", "build"]]), 3
        )

    def test_jit_label_is_not_accepted_as_bytecode_or_aot(self):
        records = "\n".join(
            f"[cross-bench] runtime=nulang-jit benchmark={name} messages=100 elapsed_ns=500"
            for name in harness.BENCHMARKS
        )
        self.assertEqual(len(harness.parse_records(records, "nulang-jit")), 4)
        for other in ("nulang", "nulang-aot"):
            with self.assertRaises(RuntimeError):
                harness.parse_records(records, other)


class JitExecutionContract(unittest.TestCase):
    def test_actor_warmup_and_tier_up_proof_precede_timing(self):
        source = (ROOT / "src/bin/nulang_savina.rs").read_text()
        runtime = (ROOT / "src/runtime/mod.rs").read_text()
        self.assertIn("Backend::Jit", source)
        self.assertIn("benchmark_jit_compiled_count()", source)
        self.assertIn("fn warm_and_verify_jit(", source)
        self.assertIn("jit_compiled_count()", runtime)
        self.assertIn("nulang-jit", source)
        for workload in ("counting", "ping_pong", "thread_ring", "fork_join"):
            self.assertIn(f'benchmark: "{workload}"', source)

    def test_jit_warmup_precedes_timed_region_in_each_fixture(self):
        source = (ROOT / "src/bin/nulang_savina.rs").read_text()
        fixtures = ("counting", "ping_pong", "thread_ring", "fork_join")
        for index, workload in enumerate(fixtures):
            start = source.index(f"fn bench_{workload}(")
            stop = (
                source.index(f"fn bench_{fixtures[index + 1]}(", start)
                if index + 1 < len(fixtures)
                else source.index("fn bench_skynet(", start)
            )
            body = source[start:stop]
            self.assertIn("if backend == Backend::Jit", body)
            self.assertLess(
                body.index('warm_and_verify_jit(&rt, "'),
                body.index("let start = Instant::now();"),
                f"{workload}: native warmup must stay outside the measured interval",
            )

    def test_warmed_jit_workflow_is_explicit(self):
        workflow = (ROOT / ".github/workflows/cross-runtime-bench.yml").read_text()
        self.assertIn("nulang-jit", workflow)


if __name__ == "__main__":
    unittest.main()
