"""Contract tests for cross-runtime stateless actor benchmark ingestion."""

import unittest

from scripts import cross_runtime_bench as bench


def fixture_records(runtime="ractor"):
    return "\n".join(
        f"[cross-bench] runtime={runtime} benchmark={name} "
        f"messages={count} elapsed_ns=1000"
        for name, count in bench.EXPECTED_MESSAGES.items()
    )


class CrossRuntimeRecordsTest(unittest.TestCase):
    def test_valid_ractor_records_are_accepted(self):
        parsed = bench.parse_records(fixture_records(), "ractor")
        self.assertEqual(
            {name: row["messages"] for name, row in parsed.items()},
            bench.EXPECTED_MESSAGES,
        )

    def test_rejects_missing_workload(self):
        rows = fixture_records().splitlines()
        with self.assertRaisesRegex(RuntimeError, "missing"):
            bench.parse_records("\n".join(rows[:-1]), "ractor")

    def test_rejects_duplicate_workload(self):
        rows = fixture_records().splitlines()
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_records("\n".join(rows + rows[:1]), "ractor")

    def test_rejects_incorrect_message_count(self):
        with self.assertRaisesRegex(RuntimeError, "message count"):
            bench.parse_records(
                fixture_records().replace("messages=200000", "messages=199999"),
                "ractor",
            )

    def test_rejects_zero_elapsed_time(self):
        with self.assertRaisesRegex(RuntimeError, "elapsed"):
            bench.parse_records(
                fixture_records().replace("elapsed_ns=1000", "elapsed_ns=0"),
                "ractor",
            )

    def test_rejects_other_runtime_records(self):
        with self.assertRaisesRegex(RuntimeError, "missing"):
            bench.parse_records(fixture_records("go"), "ractor")

    def test_rejects_missing_samples(self):
        with self.assertRaisesRegex(RuntimeError, "no samples"):
            bench.summarize({"ractor": {"counting": []}})

    def test_rejects_duplicate_selected_runtime(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            bench.validate_runtimes(["ractor", "go", "ractor"])

    def test_accepts_complete_selected_runtime_set(self):
        bench.validate_runtimes(["nulang", "rust", "go", "erlang", "ractor"])


if __name__ == "__main__":
    unittest.main()
