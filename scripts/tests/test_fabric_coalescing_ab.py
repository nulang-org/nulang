import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fabric_coalescing_ab as ab
import fabric_soak as soak


class CoalescingContract(unittest.TestCase):
    def test_collection_window_order_and_validation(self):
        self.assertEqual(ab.case_order([0, 500, 2000], 1), [2000, 500, 0])
        self.assertEqual(soak.commit_args("cross", 500), ["--cross-client-group-commit", "1", "--cross-group-wait-us", "500"])
        self.assertEqual(soak.commit_args("cross", 0), ["--cross-client-group-commit", "1"])
        for mode, wait in [("cross", -1), ("cross", 2001), ("connection", 1)]:
            with self.assertRaises(ValueError):
                soak.commit_args(mode, wait)

    def test_coalescing_metrics_are_optional_for_legacy_binary(self):
        body = "\n".join(f"fabric_{name} 0" for name in soak.METRICS)
        self.assertNotIn("coalescing_polls_total", soak.parse_metrics(body))
        parsed = soak.parse_metrics(body + "\nfabric_coalescing_polls_total 7\nfabric_coalescing_turns_total 4\n")
        self.assertEqual(parsed["coalescing_polls_total"], 7)
        self.assertEqual(parsed["coalescing_turns_total"], 4)
