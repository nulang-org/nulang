"""Contract tests for exact-base Fabric stream tail read Criterion A/B."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "fabric_stream_ab_bench.py"
spec = importlib.util.spec_from_file_location("fabric_stream_ab_bench", SCRIPT)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


def fixtures():
    base = (
        "use std::collections::HashMap;\n\n"
        "use criterion::Criterion;\n"
        "fn unrelated() {}\n\n"
        "criterion_group!(\n    benches,\n    bench_memory_journal_read\n);\n"
    )
    candidate = base.replace(
        "use std::collections::HashMap;\n",
        "use std::collections::HashMap;\n"
        "use std::fs;\n"
        "use std::sync::atomic::{AtomicUsize, Ordering};\n\n"
        "use nulang::runtime::{FabricStreamConfig, FileFabricStreamStore};\n",
    ).replace(
        "criterion_group!(\n",
        mod.FIXTURE_START + "\nfn bench_fabric_tail_read() {}\n\n"
        + "criterion_group!(\n",
    ).replace(mod.BASE_GROUP_TAIL, mod.CANDIDATE_GROUP_TAIL)
    return base, candidate


class FixtureTests(unittest.TestCase):
    def test_fixture_overlays_identical_code_on_base(self):
        base, candidate = fixtures()
        self.assertEqual(mod.matched_fixture(base, candidate), candidate)

    def test_fixture_detection_runs_only_on_newly_added_benchmark(self):
        base, candidate = fixtures()
        self.assertEqual(mod.select_fixture_mode(base, candidate), "new")
        self.assertIsNone(mod.select_fixture_mode(base, base))
        self.assertIsNone(mod.select_fixture_mode(candidate, candidate))

    def test_existing_fixture_is_skipped_for_source_only_changes(self):
        _, candidate = fixtures()
        # Source-only changes do not add a second copy of the benchmark.
        self.assertIsNone(mod.select_fixture_mode(candidate, candidate))

    def test_new_fixture_still_rejects_unrelated_benchmark_mutations(self):
        base, candidate = fixtures()
        tampered = candidate.replace("fn unrelated()", "fn mutated()")
        self.assertEqual(mod.select_fixture_mode(base, tampered), "new")
        with self.assertRaisesRegex(ValueError, "unrelated"):
            mod.matched_fixture(base, tampered)

    def test_unrelated_changes_fail_closed(self):
        base, candidate = fixtures()
        with self.assertRaisesRegex(ValueError, "unrelated"):
            mod.matched_fixture(
                base, candidate.replace("fn unrelated()", "fn modified()")
            )

    def test_missing_fixture_is_rejected(self):
        base, _ = fixtures()
        with self.assertRaisesRegex(ValueError, "fixture"):
            mod.matched_fixture(base, base)


class EstimatesTests(unittest.TestCase):
    def test_complete_criterion_estimates_are_required(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for count in mod.COUNTS:
                p = (
                    root / "criterion" / "persist_fabric_tail_read"
                    / str(count) / "new" / "estimates.json"
                )
                p.parent.mkdir(parents=True)
                p.write_text(json.dumps({
                    "mean": {"point_estimate": float(count + 1)}
                }))
            values = mod.read_estimates(root)
            self.assertEqual(set(values), {"8", "128", "1024"})
            (root / "criterion" / "persist_fabric_tail_read" / "128"
             / "new" / "estimates.json").unlink()
            with self.assertRaisesRegex(ValueError, "missing"):
                mod.read_estimates(root)

    def test_invalid_values_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "invalid"):
            mod.summarize_pairs([100, 200], [0, 400])
        with self.assertRaisesRegex(ValueError, "unequal"):
            mod.summarize_pairs([100], [101, 102])

    def test_pairs_use_round_aligned_ratios(self):
        summary = mod.summarize_pairs([100, 200, 300], [80, 250, 100])
        self.assertEqual(summary["pairs"], 3)
        self.assertAlmostEqual(summary["median_speedup_x"], 1.25)


if __name__ == "__main__":
    unittest.main()
