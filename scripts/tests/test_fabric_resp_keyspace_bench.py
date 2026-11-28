"""Keyspace benchmark trace and reply validation contracts."""
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fabric_resp_keyspace_bench as bench


class KeyspaceContract(unittest.TestCase):
    def test_trace_is_reproducible_and_uniform_reaches_multiple_keys(self):
        first = bench.trace("uniform", 32, 256, 7)
        self.assertEqual(first, bench.trace("uniform", 32, 256, 7))
        self.assertNotEqual(first, bench.trace("uniform", 32, 256, 8))
        self.assertGreater(len(set(first)), 20)
        self.assertTrue(all(0 <= k < 32 for k in first))

    def test_shared_and_skewed_workloads(self):
        self.assertEqual(bench.trace("shared", 32, 100, 7), [0] * 100)
        skew = bench.trace("hot90", 32, 10000, 7)
        self.assertTrue(8800 < skew.count(0) < 9200)
        self.assertGreater(len(set(skew)), 20)

    def test_distinct_values_and_ordered_expected_replies(self):
        self.assertNotEqual(bench.value(0), bench.value(1))
        self.assertEqual(len(bench.value(0)), 64)
        wire, expected = bench.batch([1, 0, 1])
        self.assertEqual(wire, bench.resp(b"GET", bench.key(1)) + bench.resp(b"GET", bench.key(0)) + bench.resp(b"GET", bench.key(1)))
        self.assertEqual(expected, b"".join(b"$64\r\n" + bench.value(i) + b"\r\n" for i in [1, 0, 1]))

    def test_rejects_invalid_trace_inputs(self):
        for args in [("bad", 8, 1, 0), ("uniform", 0, 1, 0), ("hot90", 1, 1, 0), ("uniform", 8, 0, 0)]:
            with self.assertRaises(ValueError):
                bench.trace(*args)

    def test_wrong_value_rejected_even_when_reply_length_matches(self):
        class Sock:
            def sendall(self, wire):
                pass
            def recv(self, length):
                return (b"$64\r\n" + bench.value(0) + b"\r\n")[:length]
        wire, expected = bench.batch([1])
        with self.assertRaisesRegex(RuntimeError, "unexpected response"):
            bench.exchange(Sock(), wire, expected)


if __name__ == "__main__":
    unittest.main()
