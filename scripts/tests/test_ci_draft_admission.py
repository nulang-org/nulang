"""Regression guard for draft-aware hosted-runner CI admission.

Keep mandatory validation for main, review-ready PRs and P0 drafts while
suppressing expensive CI jobs on ordinary drafts. A transition to ready
must retrigger the exact-head checks before merge.
"""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
CI = WORKFLOWS / "ci.yml"
CODEQL = WORKFLOWS / "codeql.yml"

REQUIRED_EVENTS = (
    "opened",
    "synchronize",
    "reopened",
    "ready_for_review",
    "converted_to_draft",
    "labeled",
    "unlabeled",
)

GATE = (
    "github.event_name != 'pull_request'",
    "github.event.pull_request.draft == false",
    "contains(github.event.pull_request.labels.*.name, 'P0')",
)


def job_source(source: str, name: str) -> str:
    """Extract one YAML job without requiring third-party YAML libraries."""
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-z][a-z0-9_-]*:\n|\Z)",
        source,
    )
    if match is None:
        raise AssertionError(f"missing workflow job: {name}")
    return match.group(1)


class DraftAwareCiContract(unittest.TestCase):
    def test_ci_retriggers_when_ready_or_p0_labeled(self):
        source = CI.read_text(encoding="utf-8")
        for event in REQUIRED_EVENTS:
            self.assertRegex(
                source,
                rf"(?m)^  pull_request:\n    types: \[.*\b{event}\b",
            )
        self.assertIn("branches: [main]", source)

    def test_all_expensive_ci_jobs_preserve_full_ready_and_p0_gate(self):
        source = CI.read_text(encoding="utf-8")
        for name in (
            "docs-site",
            "test",
            "release",
            "minimal-build",
            "wasm-backend",
            "lint",
            "lean",
        ):
            with self.subTest(job=name):
                job = job_source(source, name)
                self.assertRegex(job, r"(?m)^    if: >-$")
                for clause in GATE:
                    self.assertIn(clause, job)

    def test_package_test_inherits_release_gate_and_bench_stays_main_only(self):
        source = CI.read_text(encoding="utf-8")
        package = job_source(source, "nula-test")
        benchmark = job_source(source, "bench")
        self.assertIn("needs: release", package)
        self.assertIn("github.event_name == 'push'", benchmark)
        self.assertIn("github.ref == 'refs/heads/main'", benchmark)

    def test_codeql_keeps_main_and_schedule_and_gates_drafts(self):
        source = CODEQL.read_text(encoding="utf-8")
        self.assertIn("branches: [main]", source)
        self.assertIn("schedule:", source)
        for event in REQUIRED_EVENTS:
            self.assertRegex(
                source,
                rf"(?m)^  pull_request:\n(?:    [^\n]*\n)*?    types: \[.*\b{event}\b",
            )
        job = job_source(source, "analyze")
        self.assertRegex(job, r"(?m)^    if: >-$")
        for clause in GATE:
            self.assertIn(clause, job)


if __name__ == "__main__":
    unittest.main()
