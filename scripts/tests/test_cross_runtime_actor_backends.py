"""Fail-closed contract for compute-bearing Nulang actor backend comparisons."""
from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cross_runtime_bench import COMPUTE_BENCHMARKS, paired_backend_speedups, parse_records


class ActorComputeBackendTests(unittest.TestCase):
    def test_accepts_warmed_actor_jit_with_compilation_proof(self):
        output = (
            "[jit-warm] workload=actor_compute actor_vm_compiled_regions=1\n"
            "[cross-bench] runtime=nulang-jit benchmark=actor_compute "
            "messages=128 elapsed_ns=190000\n"
        )
        self.assertEqual(
            parse_records(output, "nulang-jit", COMPUTE_BENCHMARKS)["actor_compute"],
            {"messages": 128, "elapsed_ns": 190000},
        )

    def test_rejects_jit_enabled_without_actual_compilation(self):
        output = "[cross-bench] runtime=nulang-jit benchmark=actor_compute messages=128 elapsed_ns=1"
        with self.assertRaisesRegex(RuntimeError, "native JIT proof"):
            parse_records(output, "nulang-jit", COMPUTE_BENCHMARKS)

    def test_rejects_zero_or_duplicate_native_compilation_proof(self):
        record = "[cross-bench] runtime=nulang-jit benchmark=actor_compute messages=128 elapsed_ns=1"
        for proof in (
            "[jit-warm] workload=actor_compute actor_vm_compiled_regions=0\n",
            "[jit-warm] workload=actor_compute actor_vm_compiled_regions=1\n" * 2,
        ):
            with self.subTest(proof=proof), self.assertRaisesRegex(RuntimeError, "native JIT proof"):
                parse_records(proof + record, "nulang-jit", COMPUTE_BENCHMARKS)

    def test_rejects_duplicate_result(self):
        record = "[cross-bench] runtime=nulang-aot benchmark=actor_compute messages=128 elapsed_ns=1"
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            parse_records(record + "\n" + record, "nulang-aot", COMPUTE_BENCHMARKS)

    def test_rejects_zero_duration(self):
        record = "[cross-bench] runtime=nulang benchmark=actor_compute messages=128 elapsed_ns=0"
        with self.assertRaisesRegex(RuntimeError, "nonpositive"):
            parse_records(record, "nulang", COMPUTE_BENCHMARKS)

    def test_never_relabels_savina_counting_as_compute(self):
        record = "[cross-bench] runtime=nulang-aot benchmark=counting messages=200000 elapsed_ns=100"
        with self.assertRaisesRegex(RuntimeError, "did not emit"):
            parse_records(record, "nulang-aot", COMPUTE_BENCHMARKS)

    def test_paired_speedups_align_same_round_samples(self):
        samples = {
            "nulang": {"actor_compute": [
                {"messages": 128, "elapsed_ns": 300},
                {"messages": 128, "elapsed_ns": 600},
                {"messages": 128, "elapsed_ns": 900},
            ]},
            "nulang-jit": {"actor_compute": [
                {"messages": 128, "elapsed_ns": 100},
                {"messages": 128, "elapsed_ns": 300},
                {"messages": 128, "elapsed_ns": 300},
            ]},
            "nulang-aot": {"actor_compute": [
                {"messages": 128, "elapsed_ns": 150},
                {"messages": 128, "elapsed_ns": 300},
                {"messages": 128, "elapsed_ns": 450},
            ]},
        }
        result = paired_backend_speedups(samples)
        self.assertEqual(result["nulang-jit"]["median_speedup_vs_bytecode"], 3.0)
        self.assertEqual(result["nulang-aot"]["median_speedup_vs_bytecode"], 2.0)
        self.assertEqual(result["nulang-jit"]["paired_rounds"], 3)

    def test_paired_speedups_reject_mismatched_sample_counts(self):
        samples = {
            "nulang": {"actor_compute": [{"messages": 128, "elapsed_ns": 200}]},
            "nulang-aot": {"actor_compute": []},
        }
        with self.assertRaisesRegex(RuntimeError, "paired sample count"):
            paired_backend_speedups(samples)


if __name__ == "__main__":
    unittest.main()
