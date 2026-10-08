"""Contract tests for queue-only contention comparators.

These probes are intentionally *not* used as fair actor end-to-end throughput
rankings: Nulang uses Mailbox::push while Ractor uses ActorRef::cast.
"""
import unittest

from scripts import mailbox_contention_cross_runtime as bench


def output(runtime, producers=4, each=1000, elapsed=250_000):
    return (f"[contention-bench] runtime={runtime} producers={producers} "
            f"messages={producers*each} elapsed_ns={elapsed}\n")


class MailboxContentionContractTest(unittest.TestCase):
    def test_valid_nulang_record(self):
        r = bench.parse_record(output("nulang"), "nulang", 4, 4000)
        self.assertEqual(r["messages"], 4000)

    def test_valid_ractor_record(self):
        r = bench.parse_record(output("ractor"), "ractor", 4, 4000)
        self.assertEqual(r["elapsed_ns"], 250000)

    def test_rejects_duplicate(self):
        text = output("ractor")
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_record(text + text, "ractor", 4, 4000)

    def test_rejects_wrong_message_count(self):
        with self.assertRaisesRegex(RuntimeError, "messages"):
            bench.parse_record(output("nulang"), "nulang", 4, 4001)

    def test_rejects_zero_duration(self):
        with self.assertRaisesRegex(RuntimeError, "elapsed"):
            bench.parse_record(output("ractor", elapsed=0), "ractor", 4, 4000)

    def test_rejects_missing_runtime(self):
        with self.assertRaisesRegex(RuntimeError, "missing"):
            bench.parse_record(output("ractor"), "nulang", 4, 4000)

    def test_rejects_invalid_producer_counts(self):
        for text in ("", "0", "-1", "2,2", "xyz", "1,"):
            with self.subTest(value=text), self.assertRaises(ValueError):
                bench.parse_producers(text)

    def test_summary_rejects_no_measurements(self):
        with self.assertRaisesRegex(RuntimeError, "no samples"):
            bench.summarize([])


if __name__ == "__main__":
    unittest.main()
