"""Linux process-accounting contract for the keyspace comparison."""
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from fabric_process_metrics import read_snapshot, summarize_process


class ProcessMetricsContract(unittest.TestCase):
    def test_reads_cpu_ticks_and_rss_even_when_comm_has_parentheses(self):
        with TemporaryDirectory() as directory:
            proc = Path(directory) / "17"
            proc.mkdir()
            # Fields 3..15: state, ten other fields, utime, stime.
            (proc / "stat").write_text("17 (worker ) name) S " + "0 " * 10 + "20 5\n")
            (proc / "status").write_text("VmRSS:\t1024 kB\nVmHWM:\t2048 kB\n")
            snap = read_snapshot(17, Path(directory), ticks_per_second=100)
        self.assertEqual(snap.cpu_seconds, .25)
        self.assertEqual(snap.rss_bytes, 1024 * 1024)
        self.assertEqual(snap.peak_rss_bytes, 2048 * 1024)

    def test_reports_signed_resident_delta_and_cpu_cost(self):
        from fabric_process_metrics import Snapshot
        idle = Snapshot(0.0, 1000, 1000)
        seeded = Snapshot(.1, 3000, 4000)
        start = Snapshot(.2, 3200, 4000)
        end = Snapshot(.3, 3100, 4500)
        metrics = summarize_process(idle, seeded, start, end, keys=100, commands=1000)
        self.assertEqual(metrics["rss_after_preload_bytes"], 3000)
        self.assertEqual(metrics["rss_preload_delta_bytes_per_key"], 20)
        self.assertEqual(metrics["peak_rss_bytes"], 4500)
        self.assertAlmostEqual(metrics["server_cpu_ns_per_command"], 100000)

    def test_rejects_invalid_cpu_or_counts(self):
        from fabric_process_metrics import Snapshot
        a = Snapshot(1.0, 100, 100)
        b = Snapshot(.9, 100, 100)
        with self.assertRaises(ValueError):
            summarize_process(a, a, a, b, 10, 100)
        with self.assertRaises(ValueError):
            summarize_process(a, a, a, a, 0, 100)


if __name__ == "__main__":
    unittest.main()
