import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
BENCH_FILES = (
    ROOT / "benches" / "actor_bench.rs",
    ROOT / "benches" / "interp_bench.rs",
    ROOT / "benches" / "vm_bench.rs",
    ROOT / "benches" / "jit_bench.rs",
)
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


class BenchmarkTimingContractTests(unittest.TestCase):
    def test_owned_batched_setup_values_are_not_dropped_inside_timed_routines(self):
        for path in BENCH_FILES:
            source = path.read_text()
            with self.subTest(path=path.name):
                self.assertNotIn(
                    ".iter_batched(",
                    source,
                    "owned iter_batched includes destruction of consumed setup values "
                    "inside Criterion timing; use iter_batched_ref for VM/runtime fixtures",
                )
                self.assertIn(".iter_batched_ref(", source)

    def test_benchmark_results_are_archived_without_bypassing_protected_history(self):
        source = CI_WORKFLOW.read_text()
        archive = source.split("- name: Upload results as build artifact", 1)[1].split(
            "- name: Persist benchmark history", 1
        )[0]
        persist = source.split("- name: Persist benchmark history", 1)[1].split(
            "- name: Fail the job if paired confirmation failed operationally", 1
        )[0]

        self.assertIn("bench-results-${{ github.sha }}", archive)
        self.assertIn("benchmarks/${{ github.sha }}.json", archive)
        self.assertIn("if-no-files-found: error", archive)
        self.assertIn("retention-days: 90", archive)

        self.assertNotIn("git push", persist)
        self.assertNotIn("git commit", persist)
        self.assertIn("::warning::", persist)
        self.assertIn("automation/benchmark-history", persist)
        self.assertIn(
            "- name: Fail the job if a regression was confirmed",
            source,
            "archiving policy must not disable performance regression gating",
        )


if __name__ == "__main__":
    unittest.main()
