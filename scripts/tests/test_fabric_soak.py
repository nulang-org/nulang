"""Contracts for bounded sustained Fabric workload qualification."""
import sys
from pathlib import Path
import unittest
from unittest.mock import patch
import threading

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fabric_soak as soak


class SoakContract(unittest.TestCase):
    def test_workload_has_exact_response_and_bounded_keyspace(self):
        wire, reply, seq = soak.batch(2, 0)
        self.assertEqual(seq, 4)
        self.assertEqual(wire.count(b"*"), 16)
        self.assertIn(b":1\r\n", reply)
        self.assertIn(b":4\r\n", reply)
        self.assertIn(soak.value(4), reply)
        wire, _, _ = soak.batch(2, soak.TTL_SLOTS)
        self.assertIn(soak.ttl_key(2, 1), wire)

    def test_histogram_is_bounded_and_reports_upper_bound(self):
        histogram = soak.Histogram()
        for n in range(10000):
            histogram.add(150 if n < 9900 else 2500)
        self.assertEqual(sum(histogram.counts), 10000)
        self.assertEqual(histogram.quantile(.99), 200)
        self.assertEqual(histogram.quantile(.999), 5000)
        self.assertEqual(len(histogram.counts), len(soak.BOUNDS) + 1)

    def test_metrics_require_all_fields_and_reject_duplicate_names(self):
        body = "\n".join(f"fabric_{name} 0" for name in soak.METRICS)
        self.assertEqual(len(soak.parse_metrics(body)), len(soak.METRICS))
        with self.assertRaises(ValueError):
            soak.parse_metrics(body + "\nfabric_healthy 1")
        with self.assertRaises(ValueError):
            soak.parse_metrics("fabric_healthy 1")

    def test_sample_rejects_budget_violation(self):
        sample = {name: 0 for name in soak.METRICS}
        sample.update(healthy=1, ready=1, snapshot_fresh=1,
                      connection_budget_limit_bytes=soak.CONNECTION_BYTES,
                      journal_limit_bytes=soak.JOURNAL_BYTES, durable=1)
        soak.validate_sample(sample)
        sample["journal_bytes"] = soak.JOURNAL_BYTES + 1
        with self.assertRaises(RuntimeError):
            soak.validate_sample(sample)


    def test_worker_propagates_reply_failure_and_stops_peers(self):
        stop = threading.Event()
        barrier = threading.Barrier(1)
        with patch.object(soak.socket, "create_connection"), patch.object(
            soak, "exchange", side_effect=RuntimeError("bad reply")
        ):
            with self.assertRaisesRegex(RuntimeError, "bad reply"):
                soak.worker(("127.0.0.1", 1), 0, barrier, stop, 1, 20)
        self.assertTrue(stop.is_set())
        self.assertTrue(barrier.broken)

    def test_histogram_merge_and_overflow(self):
        a, b = soak.Histogram(), soak.Histogram()
        a.add(49)
        b.add(20_000_000)
        a.merge(b)
        self.assertEqual(a.quantile(.5), 50)
        self.assertIsNone(a.quantile(.99))
        self.assertEqual(a.report()["max_us"], 20_000_000)
        self.assertEqual(a.report()["batches"], 2)


    def test_internal_histogram_cumulative_bounds(self):
        raw = {"latency_metrics_enabled": 1}
        for name in ("command", "wal_sync", "clock_publish", "checkpoint"):
            prefix = name + "_microseconds_"
            raw.update({prefix+"count": 2, prefix+"sum": 3000001, prefix+"max": 3000000})
            for bound in soak.INTERNAL_BOUNDS:
                raw[prefix+'bucket{le="'+str(bound)+'"}'] = 1
            raw[prefix+'bucket{le="+Inf"}'] = 2
        parsed = soak.parse_latency(raw)
        self.assertEqual(parsed["wal_sync"]["p50_upper_us"], 1)
        self.assertIsNone(parsed["wal_sync"]["p99_upper_us"])
        raw['wal_sync_microseconds_bucket{le="1"}'] = 2
        with self.assertRaises(ValueError):
            soak.parse_latency(raw)


if __name__ == "__main__":
    unittest.main()
