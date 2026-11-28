import sys
from pathlib import Path
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fabric_wal_storage_ab as ab
import fabric_soak as soak


class StorageComparison(unittest.TestCase):
    def test_round_order_reverses_and_covers_matrix(self):
        first = ab.case_order(0)
        self.assertEqual(set(first), {("disk", "connection"), ("disk", "cross"), ("ram", "connection"), ("ram", "cross")})
        self.assertEqual(ab.case_order(1), list(reversed(first)))

    def test_commit_modes_are_exclusive(self):
        self.assertEqual(soak.commit_args("connection"), ["--group-commit", "1"])
        self.assertEqual(soak.commit_args("cross"), ["--cross-client-group-commit", "1"])
        with self.assertRaises(ValueError):
            soak.commit_args("buffered")

    def test_summary_requires_success_and_matching_counts(self):
        r = {"passed": True, "recovery_verified": True, "commands": 16, "ops_per_second": 16,
             "latency": {"p99_upper_us": 100, "max_us": 120},
             "precrash": {"wal_groups_synced_total": 1, "wal_group_records_synced_total": 12,
                          "checkpoint_completed_total": 1, "commands_dispatched_total": 17},
             "internal_latency": {"wal_sync": {"count": 1, "sum_us": 10, "p99_upper_us": 25},
                                  "command": {"count": 17, "sum_us": 10, "p99_upper_us": 1},
                                  "checkpoint": {"count": 1, "sum_us": 20, "p99_upper_us": 25}}}
        self.assertEqual(ab.summarize(r)["records_per_sync"], 12)
        r["internal_latency"]["wal_sync"]["count"] = 2
        with self.assertRaises(ValueError):
            ab.summarize(r)


if __name__ == "__main__":
    unittest.main()
