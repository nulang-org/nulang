import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "cross_runtime_bench.py"

spec = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
cross_runtime_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cross_runtime_bench)


def records_for(runtime="nulang", messages=10, elapsed_ns=100):
    return "\n".join(
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={messages} elapsed_ns={elapsed_ns}"
        for name in cross_runtime_bench.BENCHMARKS
    )


class RecordContractTests(unittest.TestCase):
    def test_valid_records_parse_without_changing_message_counts(self):
        result = cross_runtime_bench.parse_records(records_for(), "nulang")
        self.assertEqual(set(cross_runtime_bench.BENCHMARKS), set(result))
        self.assertEqual(10, result["ping_pong"]["messages"])

    def test_duplicate_records_fail_instead_of_silently_using_last_sample(self):
        duplicated = records_for() + "\n" + (
            "[cross-bench] runtime=nulang benchmark=ping_pong "
            "messages=99 elapsed_ns=100"
        )
        with self.assertRaisesRegex(RuntimeError, "duplicate.*ping_pong"):
            cross_runtime_bench.parse_records(duplicated, "nulang")

    def test_zero_operations_fail_before_rate_calculation(self):
        with self.assertRaisesRegex(RuntimeError, "positive"):
            cross_runtime_bench.parse_records(
                records_for(messages=0), "nulang"
            )

    def test_zero_elapsed_time_fails_before_rate_calculation(self):
        with self.assertRaisesRegex(RuntimeError, "positive"):
            cross_runtime_bench.parse_records(
                records_for(elapsed_ns=0), "nulang"
            )


class MeasurementOrderTests(unittest.TestCase):
    def test_each_runtime_occupies_every_position_in_one_rotation(self):
        runtimes = ["nulang", "rust", "go", "erlang"]
        orders = [
            cross_runtime_bench.round_order(runtimes, round_index)
            for round_index in range(len(runtimes))
        ]
        self.assertEqual(runtimes, orders[0])
        self.assertEqual(
            {name: list(range(len(runtimes))) for name in runtimes},
            {
                name: sorted(order.index(name) for order in orders)
                for name in runtimes
            },
        )
        self.assertEqual(orders[0], cross_runtime_bench.round_order(runtimes, 4))

    def test_rotation_does_not_mutate_caller_order(self):
        selected = ["nulang", "rust", "go"]
        self.assertEqual(
            ["rust", "go", "nulang"],
            cross_runtime_bench.round_order(selected, 1),
        )
        self.assertEqual(["nulang", "rust", "go"], selected)


class ComparableWorkloadsTests(unittest.TestCase):
    def test_matching_workloads_across_runtimes_are_accepted(self):
        samples = {
            runtime: {
                "counting": [
                    {"messages": 200_000, "elapsed_ns": elapsed},
                    {"messages": 200_000, "elapsed_ns": elapsed + 1},
                ]
            }
            for runtime, elapsed in (("nulang", 100), ("rust", 50))
        }
        cross_runtime_bench.validate_comparable_workloads(samples)

    def test_mismatched_counts_fail_instead_of_producing_misleading_comparison(self):
        samples = {
            "nulang": {"counting": [{"messages": 200_000, "elapsed_ns": 100}]},
            "rust": {"counting": [{"messages": 199_999, "elapsed_ns": 50}]},
        }
        with self.assertRaisesRegex(RuntimeError, "counting.*message count"):
            cross_runtime_bench.validate_comparable_workloads(samples)

    def test_incomplete_sample_round_fails(self):
        samples = {
            "nulang": {"counting": [{"messages": 10, "elapsed_ns": 100}]},
            "rust": {"counting": [
                {"messages": 10, "elapsed_ns": 100},
                {"messages": 10, "elapsed_ns": 100},
            ]},
        }
        with self.assertRaisesRegex(RuntimeError, "counting.*sample count"):
            cross_runtime_bench.validate_comparable_workloads(samples)


if __name__ == "__main__":
    unittest.main()
