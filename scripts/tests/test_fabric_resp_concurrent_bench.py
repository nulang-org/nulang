"""Concurrent benchmark accounting and failure propagation."""
import sys
from pathlib import Path
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fabric_resp_concurrent_bench as bench


class FakeSocket:
    def __init__(self, bad=False):
        self.bad = bad
        self.reply = b""

    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass

    def setsockopt(self, *args):
        pass

    def sendall(self, wire):
        count = wire.count(b"*")
        self.reply = (b"-ERR\r" if self.bad else b"+OK\r\n") * count

    def recv(self, length):
        data = self.reply[:length]
        self.reply = self.reply[length:]
        return data


class ConcurrentContract(unittest.TestCase):
    def test_counts_all_workers_and_uses_pooled_batch_latencies(self):
        with patch.object(bench.socket, "create_connection", side_effect=lambda *a, **k: FakeSocket()):
            result = bench.measure_concurrent(("127.0.0.1", 1), "set", 4, 8, 3)
        self.assertEqual(result["commands"], 24)
        self.assertEqual(result["batches"], 6)
        self.assertEqual(result["clients"], 3)
        self.assertEqual(len(result["client_completion_ms"]), 3)
        self.assertGreater(result["ops_per_second"], 0)
        self.assertGreaterEqual(result["batch_p99_us"], result["batch_p50_us"])

    def test_bad_response_aborts_instead_of_reporting_partial_sample(self):
        with patch.object(bench.socket, "create_connection", side_effect=lambda *a, **k: FakeSocket(bad=True)):
            with self.assertRaisesRegex(RuntimeError, "unexpected response"):
                bench.measure_concurrent(("127.0.0.1", 1), "set", 4, 8, 3)

    def test_rejects_invalid_counts(self):
        for depth, commands, clients in [(0, 8, 3), (4, 7, 3), (4, 8, 0), (4, 8, 64)]:
            with self.assertRaises(ValueError):
                bench.measure_concurrent(("127.0.0.1", 1), "set", depth, commands, clients)


if __name__ == "__main__":
    unittest.main()
