import importlib.util
import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "trace_dispatch_abc_bench.py"
spec = importlib.util.spec_from_file_location("trace_dispatch_abc_bench", SCRIPT)
abc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(abc)


class ExperimentPlanTests(unittest.TestCase):
    def test_each_variant_occupies_each_execution_position_twice(self):
        rounds = [abc.round_order(i) for i in range(6)]
        self.assertEqual(len({tuple(r) for r in rounds}), 6)
        for r in rounds:
            self.assertEqual(set(r), {"base", "optimized", "eager_root"})
        for variant in ("base", "optimized", "eager_root"):
            for pos in range(3):
                self.assertEqual(sum(r[pos] == variant for r in rounds), 2)

    def test_require_exact_sha_rejects_branch_and_short_ref(self):
        for invalid in ["main", "a" * 7, "a" * 39, "z" * 40]:
            with self.assertRaisesRegex(ValueError, "40-character commit SHA"):
                abc.require_sha(invalid)
        self.assertEqual(abc.require_sha("a" * 40), "a" * 40)

    def test_cargo_targets_isolated_for_three_variants(self):
        envs = [abc.cargo_environment(v) for v in ("base", "optimized", "eager_root")]
        self.assertEqual(len({env["CARGO_TARGET_DIR"] for env in envs}), 3)


class SampleContracts(unittest.TestCase):
    def test_parser_requires_all_targeted_benchmark_records(self):
        sample = '\n'.join([
            f'[cross-bench] runtime=nulang benchmark={name} messages=100 elapsed_ns=100'
            for name in ("ping_pong", "thread_ring", "fork_join")
        ]) + '\n' + '\n'.join([
            f'[ab-bench] benchmark={name} operations=100 elapsed_ns=100'
            for name in ("enqueue_payload_0", "jit_warm_hot_loop_100k", "mailbox_push_inline_1")
        ])
        self.assertEqual(set(abc.parse_targets(sample)), set(abc.TARGETS))
        with self.assertRaisesRegex(RuntimeError, "missing benchmark records"):
            abc.parse_targets("[cross-bench] runtime=nulang benchmark=ping_pong messages=1 elapsed_ns=10")

    def test_three_way_paired_ratios_use_same_rounds(self):
        samples = {
            "base": {"ab/enqueue_payload_0": [(100, 100), (100, 200), (100, 300)]},
            "optimized": {"ab/enqueue_payload_0": [(100, 80), (100, 160), (100, 240)]},
            "eager_root": {"ab/enqueue_payload_0": [(100, 100), (100, 200), (100, 300)]},
        }
        result = abc.paired_results(samples)
        self.assertAlmostEqual(result["ab/enqueue_payload_0"]["optimized_vs_base"]["median_speedup_x"], 1.25)
        self.assertAlmostEqual(result["ab/enqueue_payload_0"]["eager_root_vs_base"]["median_speedup_x"], 1.0)
        self.assertAlmostEqual(result["ab/enqueue_payload_0"]["optimized_vs_eager_root"]["median_speedup_x"], 1.25)

    def test_disallow_mismatched_counts_and_nonpositive_time(self):
        sample = {
            "base": {"case": [(100, 100)]},
            "optimized": {"case": [(101, 90)]},
            "eager_root": {"case": [(100, 100)]},
        }
        with self.assertRaisesRegex(RuntimeError, "operation count"):
            abc.paired_results(sample)
        sample["optimized"]["case"] = [(100, 0)]
        with self.assertRaisesRegex(RuntimeError, "positive"):
            abc.paired_results(sample)
        sample["optimized"]["case"] = [(100, 100), (100, 100)]
        with self.assertRaisesRegex(RuntimeError, "round count"):
            abc.paired_results(sample)


if __name__ == "__main__":
    unittest.main()
