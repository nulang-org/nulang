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

    def test_benchmark_history_persists_all_canonical_snapshots(self):
        source = CI_WORKFLOW.read_text()
        persist = source.split("- name: Persist benchmark history", 1)[1].split(
            "- name: Fail the job if paired confirmation failed operationally", 1
        )[0]

        self.assertNotIn(
            'git add "benchmarks/${{ github.sha }}.json"',
            persist,
            "persisting only the current SHA drops previously restored benchmark history",
        )
        self.assertIn(
            "[0-9a-f]{40}",
            persist,
            "history persistence must stage every canonical 40-hex-SHA snapshot",
        )
        self.assertIn(
            "git add --",
            persist,
            "canonical benchmark snapshots must be staged before the history branch is pushed",
        )


if __name__ == "__main__":
    unittest.main()
