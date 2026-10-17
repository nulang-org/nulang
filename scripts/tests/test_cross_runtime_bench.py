"""Fail closed on malformed, duplicate or semantically invalid benchmark output."""
import importlib.util
import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "cross_runtime_bench.py"
spec = importlib.util.spec_from_file_location("cross_runtime_bench", SCRIPT)
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

COUNTS = {
    "counting": 200_000,
    "ping_pong": 40_001,
    "thread_ring": 20_000,
    "fork_join": 100_000,
}


def fixture(runtime="rust", *, counts=None, elapsed=1000):
    counts = counts or COUNTS
    return "\n".join(
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={counts[name]} elapsed_ns={elapsed}"
        for name in COUNTS
    )


class CrossRuntimeRecordTests(unittest.TestCase):
    def test_accepts_exact_complete_fixture_with_non_benchmark_logs(self):
        records = bench.parse_records("banner\n" + fixture() + "\nfinal log", "rust")
        self.assertEqual(set(COUNTS), set(records))
        self.assertEqual(COUNTS["ping_pong"], records["ping_pong"]["messages"])

    def test_rejects_duplicate_workload_instead_of_overwriting_sample(self):
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_records(fixture() + "\n" + fixture().splitlines()[0], "rust")

    def test_rejects_forged_workload_count_even_if_all_four_present(self):
        counts = dict(COUNTS, counting=199_999)
        with self.assertRaisesRegex(RuntimeError, "message count"):
            bench.parse_records(fixture(counts=counts), "rust")

    def test_rejects_zero_elapsed_timing(self):
        with self.assertRaisesRegex(RuntimeError, "elapsed_ns"):
            bench.parse_records(fixture(elapsed=0), "rust")

    def test_rejects_malformed_benchmark_record(self):
        with self.assertRaisesRegex(RuntimeError, "malformed"):
            bench.parse_records(fixture() + "\n[cross-bench] runtime=rust bad-data", "rust")

    def test_rejects_incorrect_runtime_label(self):
        with self.assertRaisesRegex(RuntimeError, "runtime"):
            bench.parse_records(fixture("erlang"), "rust")

    def test_allows_additional_valid_noncomparable_workload(self):
        records = bench.parse_records(
            fixture() + "\n[cross-bench] runtime=rust benchmark=skynet messages=1 elapsed_ns=3",
            "rust",
        )
        self.assertEqual(set(COUNTS), set(records))

    def test_rejects_missing_workload(self):
        with self.assertRaisesRegex(RuntimeError, "did not emit"):
            bench.parse_records("\n".join(fixture().splitlines()[:-1]), "rust")


if __name__ == "__main__":
    unittest.main()
