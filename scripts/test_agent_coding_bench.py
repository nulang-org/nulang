"""Regression tests for deterministic coding-agent benchmark scoring."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("agent_coding_bench.py")


def record(model, task, trial, compiler=True, tests=True, repairs=0, tokens=100, ms=500):
    return {
        "model": model,
        "task_id": task,
        "trial": trial,
        "compiler_ok": compiler,
        "tests_ok": tests,
        "repair_cycles": repairs,
        "input_tokens": tokens,
        "output_tokens": 20,
        "elapsed_ms": ms,
    }


class AgentCodingBenchTests(unittest.TestCase):
    def run_report(self, tasks, rows, trials=2):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "tasks.json").write_text(json.dumps({
                "schema_version": 1,
                "suite_id": "test-suite",
                "trials": trials,
                "tasks": [{"id": t} for t in tasks],
            }), encoding="utf-8")
            (root / "results.jsonl").write_text(
                "".join(json.dumps(row) + "\n" for row in rows),
                encoding="utf-8",
            )
            process = subprocess.run(
                [sys.executable, str(SCRIPT), "--tasks", str(root / "tasks.json"),
                 "--results", str(root / "results.jsonl")],
                capture_output=True, text=True, check=False,
            )
            return process, json.loads(process.stdout) if process.returncode == 0 else None

    def test_full_coverage_and_attempt_cost_are_deterministic(self):
        rows = [
            record("B", "one", 1, compiler=False, repairs=2, tokens=200, ms=1500),
            record("A", "one", 1, repairs=1, tokens=100),
            record("A", "two", 1, compiler=True, tests=False, tokens=50),
            record("A", "one", 2, repairs=0, tokens=80),
            record("A", "two", 2, repairs=1, tokens=90),
        ]
        proc, report = self.run_report(["one", "two"], rows)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertRegex(report["manifest_sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual([m["model"] for m in report["models"]], ["A", "B"])
        a = report["models"][0]
        self.assertEqual(a["completed"], 4)
        self.assertEqual(a["expected"], 4)
        self.assertEqual(a["successes"], 3)
        self.assertEqual(a["coverage"], 1.0)
        self.assertEqual(a["success_rate_all_tasks"], 0.75)
        self.assertEqual(a["tokens_per_success"], 400 / 3)
        self.assertEqual(a["median_repair_cycles_success"], 1)
        b = report["models"][1]
        self.assertEqual(b["coverage"], 0.25)
        self.assertEqual(b["success_rate_all_tasks"], 0.0)
        self.assertIsNone(b["tokens_per_success"])

    def test_rejects_duplicate_model_task_trial(self):
        row = record("A", "one", 1)
        proc, _ = self.run_report(["one"], [row, row], trials=1)
        self.assertEqual(proc.returncode, 2)
        self.assertIn("duplicate", proc.stderr)

    def test_rejects_unknown_task_and_invalid_trial(self):
        proc, _ = self.run_report(["one"], [record("A", "other", 1)])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("unknown task", proc.stderr)
        proc, _ = self.run_report(["one"], [record("A", "one", 3)])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("trial", proc.stderr)

    def test_rejects_boolean_token_counts_and_negative_latency(self):
        bad = record("A", "one", 1)
        bad["input_tokens"] = True
        proc, _ = self.run_report(["one"], [bad])
        self.assertEqual(proc.returncode, 2)
        bad = record("A", "one", 1, ms=-1)
        proc, _ = self.run_report(["one"], [bad])
        self.assertEqual(proc.returncode, 2)

    def test_reports_partial_coverage_without_silent_success_inflation(self):
        proc, report = self.run_report(["one", "two"], [record("A", "one", 1)])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        a = report["models"][0]
        self.assertEqual(a["successes"], 1)
        self.assertEqual(a["completed"], 1)
        self.assertEqual(a["expected"], 4)
        self.assertEqual(a["coverage"], 0.25)
        self.assertEqual(a["success_rate_observed"], 1.0)
        self.assertEqual(a["success_rate_all_tasks"], 0.25)

    def test_manifest_changes_produce_distinct_fingerprints(self):
        proc_one, report_one = self.run_report(
            ["one"], [record("A", "one", 1)], trials=1
        )
        proc_two, report_two = self.run_report(
            ["one", "two"], [record("A", "one", 1)], trials=1
        )
        self.assertEqual(proc_one.returncode, 0)
        self.assertEqual(proc_two.returncode, 0)
        self.assertNotEqual(
            report_one["manifest_sha256"], report_two["manifest_sha256"]
        )

    def test_empty_results_fail_instead_of_fabricating_scores(self):
        proc, _ = self.run_report(["one"], [])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("no results", proc.stderr)


if __name__ == "__main__":
    unittest.main()
