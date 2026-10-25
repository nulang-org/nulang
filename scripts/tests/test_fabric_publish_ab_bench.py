"""Contract tests for identical-fixture sparse Fabric A/B measurements."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "fabric_publish_ab_bench.py"
spec = importlib.util.spec_from_file_location("fabric_publish_ab_bench", SCRIPT)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


def fixture_sources():
    base = "fn other() {}\n\ncriterion_group!(\n    benches,\n    bench_selective_receive\n);\n"
    extra = mod.START + "\nfn bench_fabric_publish_unmatched() {}\n\n"
    candidate = base.replace("criterion_group!(", extra + "criterion_group!(")
    candidate = candidate.replace(mod.BASE_GROUP_TAIL, mod.CANDIDATE_GROUP_TAIL)
    return base, candidate


def exact_fixture_sources():
    # The previously merged unmatched-actor fixture remains in main.
    _, base = fixture_sources()
    block = (
        "/// Unmatched publication with many unrelated exact-subject subscriptions.\n"
        "fn bench_fabric_publish_unmatched_exact_subscriptions() {}\n\n"
    )
    candidate = base.replace("criterion_group!(", block + "criterion_group!(")
    candidate = candidate.replace(
        "    bench_fabric_publish_unmatched\n);",
        "    bench_fabric_publish_unmatched,\n"
        "    bench_fabric_publish_unmatched_exact_subscriptions\n);",
    )
    return base, candidate


class FixtureContractTests(unittest.TestCase):
    def test_only_identical_fabric_fixture_can_be_overlaid(self):
        base, candidate = fixture_sources()
        self.assertEqual(candidate, mod.matched_fixture(base, candidate))

    def test_unrelated_benchmark_edit_fails_closed(self):
        base, candidate = fixture_sources()
        with self.assertRaisesRegex(RuntimeError, "differ beyond"):
            mod.matched_fixture(base, candidate.replace("fn other()", "fn changed()"))

    def test_missing_fixture_fails_closed(self):
        base, _ = fixture_sources()
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            mod.matched_fixture(base, base)


    def test_exact_subscription_fixture_is_identical_on_both_variants(self):
        base, candidate = exact_fixture_sources()
        self.assertEqual(
            candidate, mod.matched_fixture(base, candidate, mode="exact")
        )

    def test_exact_subscription_fixture_rejects_unrelated_changes(self):
        base, candidate = exact_fixture_sources()
        with self.assertRaisesRegex(RuntimeError, "differ beyond"):
            mod.matched_fixture(
                base, candidate.replace("fn other()", "fn changed()"),
                mode="exact",
            )

    def test_exact_subscription_fixture_rejects_missing_block(self):
        base, _ = exact_fixture_sources()
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            mod.matched_fixture(base, base, mode="exact")

    def test_fixture_mode_selection_uses_newly_added_fixture_only(self):
        older_base, first_candidate = fixture_sources()
        base, exact_candidate = exact_fixture_sources()
        self.assertEqual(mod.select_mode(older_base, first_candidate), "idle")
        self.assertEqual(mod.select_mode(base, exact_candidate), "exact")
        self.assertIsNone(mod.select_mode(base, base))
        self.assertIsNone(mod.select_mode(exact_candidate, exact_candidate))



class ResultContractTests(unittest.TestCase):
    def test_results_require_all_actor_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for n in mod.COUNTS:
                path = root / "criterion" / "actor_fabric_publish_unmatched" / str(n) / "new" / "estimates.json"
                path.parent.mkdir(parents=True)
                path.write_text(json.dumps({"mean": {"point_estimate": float(n + 1)}}))
            result = mod.read_estimates(root)
            self.assertEqual(set(result), {"0", "1000", "10000"})
            (root / "criterion" / "actor_fabric_publish_unmatched" / "1000" / "new" / "estimates.json").unlink()
            with self.assertRaisesRegex(RuntimeError, "missing"):
                mod.read_estimates(root)

    def test_exact_subscription_results_require_all_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            group = "actor/fabric_publish_unmatched_exact_subscriptions"
            counts = (0, 256, 2048)
            for count in counts:
                folder = group.replace("/", "_")
                path = root / "criterion" / folder / str(count) / "new" / "estimates.json"
                path.parent.mkdir(parents=True)
                path.write_text(json.dumps({"mean": {"point_estimate": float(count + 1)}}))
            result = mod.read_estimates(root, counts=counts, benchmark_group=group)
            self.assertEqual(set(result), {"0", "256", "2048"})
            missing = root / "criterion" / group.replace("/", "_") / "256" / "new" / "estimates.json"
            missing.unlink()
            with self.assertRaisesRegex(RuntimeError, "missing"):
                mod.read_estimates(root, counts=counts, benchmark_group=group)

    def test_pairing_is_round_aligned_not_ratio_of_independent_medians(self):
        result = mod.summarize_pairs([100, 200, 400], [80, 250, 200])
        self.assertEqual(result["pairs"], 3)
        self.assertAlmostEqual(result["median_speedup_x"], 1.25)

    def test_pairing_fails_closed_for_incomplete_or_invalid_runs(self):
        with self.assertRaisesRegex(RuntimeError, "unequal"):
            mod.summarize_pairs([100, 200], [150])
        with self.assertRaisesRegex(RuntimeError, "nonpositive"):
            mod.summarize_pairs([100], [0])


if __name__ == "__main__":
    unittest.main()
