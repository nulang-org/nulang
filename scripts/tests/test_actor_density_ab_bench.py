import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "actor_density_ab_bench.py"

spec = importlib.util.spec_from_file_location("actor_density_ab_bench", SCRIPT)
actor_density_ab_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(actor_density_ab_bench)


class ParseProbeTests(unittest.TestCase):
    def test_parse_construct_probe(self):
        output = """actor_struct_bytes=2432
mode=construct
actor_count=10000
construct_seconds=0.050000
construct_per_second=200000
construct_ns_per_item=5000.0
rss_before_bytes=1000000
rss_after_construct_bytes=25000000
rss_construct_delta_bytes=24000000
rss_construct_delta_bytes_per_actor=2400.0
"""
        row = actor_density_ab_bench.parse_probe(output, "construct")
        self.assertEqual(10000, row["actor_count"])
        self.assertEqual(2432, row["actor_struct_bytes"])
        self.assertEqual(5000.0, row["ns_per_actor"])
        self.assertEqual(2400.0, row["rss_bytes_per_actor"])

    def test_parse_idle_probe(self):
        output = """actor_struct_bytes=2432
mode=idle
actor_count=10000
spawn_seconds=0.060000
spawn_per_second=166667
spawn_ns_per_item=6000.0
scheduler_settle_seconds=0.004000
actor_heap_used_bytes=0
actor_heap_used_bytes_per_actor=0.0
rss_before_bytes=1000000
rss_after_spawn_bytes=37000000
rss_after_settle_bytes=37000000
rss_spawn_delta_bytes=36000000
rss_spawn_delta_bytes_per_actor=3600.0
"""
        row = actor_density_ab_bench.parse_probe(output, "idle")
        self.assertEqual(6000.0, row["ns_per_actor"])
        self.assertEqual(3600.0, row["rss_bytes_per_actor"])
        self.assertEqual(0.004, row["scheduler_settle_seconds"])
        self.assertEqual(0.0, row["actor_heap_used_bytes_per_actor"])

    def test_parse_unavailable_rss_as_none(self):
        output = """actor_struct_bytes=2432
mode=idle
actor_count=10000
spawn_ns_per_item=6000.0
scheduler_settle_seconds=0.004000
actor_heap_used_bytes_per_actor=0.0
rss_spawn_delta_bytes_per_actor=unavailable
"""
        row = actor_density_ab_bench.parse_probe(output, "idle")
        self.assertIsNone(row["rss_bytes_per_actor"])

    def test_parse_rejects_wrong_mode(self):
        output = "mode=construct\nactor_count=10000\nconstruct_ns_per_item=5000.0\nactor_struct_bytes=2432\nrss_construct_delta_bytes_per_actor=2400.0\n"
        with self.assertRaisesRegex(RuntimeError, "expected mode idle"):
            actor_density_ab_bench.parse_probe(output, "idle")


class ComparisonTests(unittest.TestCase):
    def test_paired_lower_is_better_reports_speedup_and_ci(self):
        base = [100.0, 110.0, 90.0]
        candidate = [80.0, 100.0, 75.0]
        result = actor_density_ab_bench.paired_lower_is_better(base, candidate)
        self.assertEqual(3, result["pairs"])
        self.assertGreater(result["median_improvement_x"], 1.0)
        self.assertGreater(result["median_reduction_pct"], 0.0)
        self.assertLessEqual(result["ci95_lower"], result["median_improvement_x"])
        self.assertGreaterEqual(result["ci95_upper"], result["median_improvement_x"])

    def test_paired_lower_is_better_rejects_misaligned_samples(self):
        with self.assertRaisesRegex(RuntimeError, "paired sample count changed"):
            actor_density_ab_bench.paired_lower_is_better([1.0, 2.0], [1.0])

    def test_comparisons_treat_zero_rss_delta_as_unavailable(self):
        samples = {
            "base": {
                "construct": [{"ns_per_actor": 100.0, "rss_bytes_per_actor": 0.0, "actor_struct_bytes": 2800, "actor_count": 10000}],
                "idle": [{"ns_per_actor": 200.0, "rss_bytes_per_actor": 4000.0, "actor_struct_bytes": 2800, "actor_count": 10000}],
            },
            "candidate": {
                "construct": [{"ns_per_actor": 90.0, "rss_bytes_per_actor": 0.0, "actor_struct_bytes": 2400, "actor_count": 10000}],
                "idle": [{"ns_per_actor": 180.0, "rss_bytes_per_actor": 3600.0, "actor_struct_bytes": 2400, "actor_count": 10000}],
            },
        }

        result = actor_density_ab_bench.comparisons(samples)
        self.assertIsNone(result["construct"]["rss"])
        self.assertIsNotNone(result["idle"]["rss"])

    def test_measurement_order_counterbalances_variant_and_mode_order(self):
        self.assertEqual(
            [("base", "construct"), ("base", "idle"), ("candidate", "construct"), ("candidate", "idle")],
            actor_density_ab_bench.measurement_order(0),
        )
        self.assertEqual(
            [("candidate", "idle"), ("candidate", "construct"), ("base", "idle"), ("base", "construct")],
            actor_density_ab_bench.measurement_order(1),
        )


if __name__ == "__main__":
    unittest.main()
