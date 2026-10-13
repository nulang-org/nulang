"""Fail-closed workload and measurement contract for cross-runtime comparisons."""

import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "cross_runtime_bench.py"
spec = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
cross_runtime_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cross_runtime_bench)

ROWS = {
    "counting": 200_000,
    "ping_pong": 40_001,
    "thread_ring": 20_000,
    "fork_join": 100_000,
}


def record(runtime: str, name: str, messages: int, elapsed_ns: int = 1000) -> str:
    return (
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={messages} elapsed_ns={elapsed_ns}"
    )


def valid_output(runtime: str = "nulang") -> str:
    return "\n".join(record(runtime, name, count) for name, count in ROWS.items())


class BenchmarkRecordContractTests(unittest.TestCase):
    def test_accepts_one_complete_valid_set(self):
        records = cross_runtime_bench.parse_records(valid_output(), "nulang")
        self.assertEqual(set(ROWS), set(records))
        for name, count in ROWS.items():
            self.assertEqual(count, records[name]["messages"])
            self.assertEqual(1000, records[name]["elapsed_ns"])

    def test_rejects_duplicate_workload_in_one_process_output(self):
        output = valid_output() + "\n" + record("nulang", "counting", ROWS["counting"])
        with self.assertRaisesRegex(RuntimeError, "duplicate.*counting"):
            cross_runtime_bench.parse_records(output, "nulang")

    def test_rejects_wrong_runtime_instead_of_silently_ignoring_it(self):
        output = valid_output() + "\n" + record("go", "counting", ROWS["counting"])
        with self.assertRaisesRegex(RuntimeError, "unexpected runtime"):
            cross_runtime_bench.parse_records(output, "nulang")

    def test_accepts_nulang_only_skynet_without_reporting_it(self):
        # The standalone Nulang Savina runner emits a fifth, unmatched workload.
        output = valid_output() + "\\n" + record("nulang", "skynet", 1110)
        records = cross_runtime_bench.parse_records(output, "nulang")
        self.assertEqual(set(ROWS), set(records))
        self.assertNotIn("skynet", records)

    def test_rejects_duplicate_nulang_only_skynet_record(self):
        output = valid_output() + "\\n" + record("nulang", "skynet", 1110)
        output += "\\n" + record("nulang", "skynet", 1110)
        with self.assertRaisesRegex(RuntimeError, "duplicate.*skynet"):
            cross_runtime_bench.parse_records(output, "nulang")

    def test_rejects_changed_nulang_only_skynet_count(self):
        output = valid_output() + "\\n" + record("nulang", "skynet", 1109)
        with self.assertRaisesRegex(RuntimeError, "skynet.*expected 1110"):
            cross_runtime_bench.parse_records(output, "nulang")

    def test_rejects_unknown_workload_instead_of_silently_ignoring_it(self):
        output = valid_output() + "\n" + record("nulang", "surprise", 100)
        with self.assertRaisesRegex(RuntimeError, "unexpected benchmark"):
            cross_runtime_bench.parse_records(output, "nulang")

    def test_rejects_changed_logical_message_count(self):
        rows = [
            record("nulang", name, count - 1 if name == "ping_pong" else count)
            for name, count in ROWS.items()
        ]
        with self.assertRaisesRegex(RuntimeError, "ping_pong.*expected 40001"):
            cross_runtime_bench.parse_records("\n".join(rows), "nulang")

    def test_rejects_non_positive_elapsed_time(self):
        rows = [
            record("nulang", name, count, 0 if name == "thread_ring" else 1000)
            for name, count in ROWS.items()
        ]
        with self.assertRaisesRegex(RuntimeError, "thread_ring.*positive"):
            cross_runtime_bench.parse_records("\n".join(rows), "nulang")

    def test_rejects_missing_workload(self):
        output = "\n".join(
            record("nulang", name, count)
            for name, count in ROWS.items()
            if name != "fork_join"
        )
        with self.assertRaisesRegex(RuntimeError, "fork_join"):
            cross_runtime_bench.parse_records(output, "nulang")


class MeasurementOrderTests(unittest.TestCase):
    def test_rotates_all_runtimes_through_every_order_position(self):
        selected = ["nulang", "rust", "go", "erlang"]
        orders = [
            cross_runtime_bench.measurement_order(selected, i)
            for i in range(len(selected))
        ]
        self.assertEqual(selected, orders[0])
        for runtime in selected:
            positions = [round_order.index(runtime) for round_order in orders]
            self.assertEqual(list(range(len(selected))), sorted(positions))

    def test_selected_runtime_list_is_not_modified(self):
        selected = ["nulang", "go"]
        self.assertEqual(["go", "nulang"], cross_runtime_bench.measurement_order(selected, 1))
        self.assertEqual(["nulang", "go"], selected)


if __name__ == "__main__":
    unittest.main()
