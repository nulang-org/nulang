"""Wire correctness and latency-report contract for the RESP A/B baseline."""
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "fabric_resp_ab_bench.py"
spec = importlib.util.spec_from_file_location("fabric_resp_ab_bench", SCRIPT)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


class FakeSocket:
    def __init__(self, replies):
        self.replies = bytearray(replies)
        self.sent = []

    def sendall(self, data):
        self.sent.append(data)

    def recv(self, n):
        chunk = bytes(self.replies[:min(n, 3)])
        del self.replies[:len(chunk)]
        return chunk


class RespBenchmarkContract(unittest.TestCase):
    def test_binary_safe_frames_and_fragmented_reply(self):
        self.assertEqual(mod.resp(b"SET", b"k", b"\x00\r\n"), b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\n\x00\r\n\r\n")
        sock = FakeSocket(mod.HIT * 2)
        mod.exchange(sock, b"request", mod.HIT * 2)
        self.assertEqual(sock.sent, [b"request"])
        self.assertEqual(sock.replies, b"")

    def test_wrong_and_truncated_replies_fail(self):
        with self.assertRaisesRegex(RuntimeError, "unexpected response"):
            mod.exchange(FakeSocket(b"-ERR\r"), b"request", mod.OK)
        with self.assertRaisesRegex(RuntimeError, "disconnected"):
            mod.exchange(FakeSocket(b"+O"), b"request", mod.OK)

    def test_pipeline_measurement_counts_exact_batches_and_commands(self):
        sock = FakeSocket(mod.OK + mod.HIT * 16)
        result = mod.measure(sock, "get", 4, 8)
        self.assertEqual(result["commands"], 8)
        self.assertEqual(result["batches"], 2)
        self.assertEqual(len(sock.sent), 5)  # seed, two warmups, two measured batches
        self.assertTrue(result["ops_per_second"] > 0)
        with self.assertRaises(ValueError):
            mod.measure(sock, "get", 4, 7)

    def test_nearest_rank_and_median(self):
        self.assertEqual(mod.percentile([30, 10, 20], .5), 20)
        self.assertEqual(mod.percentile([30, 10, 20], .999), 30)
        self.assertEqual(mod.summarize([{"x": 2}, {"x": 10}, {"x": 4}]), {"x": 4})


if __name__ == "__main__":
    unittest.main()
