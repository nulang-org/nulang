"""Contracts for Nulang/Ractor actor creation and resident-memory measurements."""
import unittest

from scripts import actor_density_cross_runtime as bench


class DensityOutputTests(unittest.TestCase):
    def fixture(self, *, runtime="nulang", actors=1_000):
        return "\n".join([
            "mode=idle",
            *(["runtime=ractor"] if runtime == "ractor" else []),
            f"actor_count={actors}",
            "spawn_ns_per_item=2300.0",
            "scheduler_settle_seconds=0.010000",
            "rss_before_bytes=4096000",
            "rss_after_spawn_bytes=8192000",
            "rss_after_settle_bytes=12288000",
        ])

    def test_parses_nulang_spawn_and_settled_memory(self):
        row = bench.parse_probe(self.fixture(), "nulang", 1000)
        self.assertEqual(row["spawn_ns_per_actor"], 2300.0)
        self.assertEqual(row["settled_rss_bytes_per_actor"], 8192.0)
        self.assertEqual(row["spawn_rss_bytes_per_actor"], 4096.0)

    def test_parses_ractor(self):
        row = bench.parse_probe(self.fixture(runtime="ractor"), "ractor", 1000)
        self.assertEqual(row["runtime"], "ractor")

    def test_rejects_wrong_count(self):
        with self.assertRaisesRegex(RuntimeError, "actor count"):
            bench.parse_probe(self.fixture(actors=2000), "nulang", 1000)

    def test_rejects_duplicate_output_field(self):
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            bench.parse_probe(self.fixture() + "\nspawn_ns_per_item=12", "nulang", 1000)

    def test_rejects_nan_and_zero_time(self):
        for number in ("nan", "0.0", "-1.0", "inf"):
            with self.subTest(value=number):
                with self.assertRaisesRegex(RuntimeError, "spawn_ns_per_item"):
                    bench.parse_probe(
                        self.fixture().replace("2300.0", number), "nulang", 1000
                    )

    def test_missing_rss_is_not_zero_memory(self):
        row = bench.parse_probe(
            self.fixture().replace("12288000", "unavailable"), "nulang", 1000
        )
        self.assertIsNone(row["settled_rss_bytes_per_actor"])

    def test_rejects_declining_rss_as_unavailable(self):
        row = bench.parse_probe(
            self.fixture().replace("12288000", "2048000"), "nulang", 1000
        )
        self.assertIsNone(row["settled_rss_bytes_per_actor"])

    def test_rejects_conflicting_runtime(self):
        with self.assertRaisesRegex(RuntimeError, "runtime"):
            bench.parse_probe(self.fixture(runtime="ractor"), "nulang", 1000)

    def test_summary_rejects_empty_samples(self):
        with self.assertRaisesRegex(RuntimeError, "no samples"):
            bench.summarize([])

    def test_summary_skips_unavailable_rss(self):
        first = bench.parse_probe(self.fixture(), "nulang", 1000)
        missing = dict(first, settled_rss_bytes_per_actor=None)
        row = bench.summarize([first, missing])
        self.assertEqual(row["median_settled_rss_bytes_per_actor"], 8192.0)

    def test_actor_counts_validation(self):
        self.assertEqual(bench.parse_actor_counts("1000, 10000"), [1000, 10000])
        for value in ("", "0", "1000,1000", "-1", "not-a-number", "1000,"):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    bench.parse_actor_counts(value)


if __name__ == "__main__":
    unittest.main()
