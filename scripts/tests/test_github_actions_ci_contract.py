"""Guard the GitHub Actions CI admission and concurrency contract."""

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


class GitHubActionsCIContractTests(unittest.TestCase):
    def test_primary_ci_runs_on_pull_requests_and_main_pushes(self):
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn("name: CI\n", source)
        self.assertIn("  push:\n    branches: [main]\n", source)
        self.assertIn("  pull_request:\n", source)

    def test_superseded_runs_cancel_per_ref_not_globally(self):
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            "concurrency:\n"
            "  group: ci-${{ github.workflow }}-${{ github.ref }}\n"
            "  cancel-in-progress: true\n",
            source,
            "CI should cancel superseded runs for the same PR/branch only",
        )
        self.assertNotIn(
            "group: ci-${{ github.workflow }}\n",
            source,
            "A repository-wide concurrency key serializes unrelated pull requests",
        )

    def test_rust_build_matrix_remains_in_primary_ci(self):
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        # Every Rust test job runs under nextest so the retry policy in
        # .config/nextest.toml applies uniformly; doctests stay on cargo test.
        self.assertIn("cargo nextest run --locked", source)
        self.assertIn("cargo nextest run --locked --features wasm-backend", source)
        self.assertIn("cargo nextest run --locked --no-default-features", source)
        self.assertIn(
            "cargo nextest run --locked --release --no-default-features --features release-cli",
            source,
        )
        self.assertIn("cargo clippy --locked --all-targets", source)
        self.assertIn("--doc", source)

    def test_whitespace_parity_step_is_retained(self):
        source = CI_WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            "git diff --check",
            source,
            "the retired Woodpecker pipeline's whitespace check must stay enforced",
        )

    def test_known_flaky_cluster_tests_have_scoped_retries(self):
        policy = (ROOT / ".config" / "nextest.toml").read_text(encoding="utf-8")

        self.assertIn("cluster_split_brain", policy)
        self.assertIn("retries = 2", policy)

    def test_pull_request_workflows_cancel_superseded_runs(self):
        workflow_dir = ROOT / ".github" / "workflows"

        for path in sorted(workflow_dir.glob("*.yml")):
            source = path.read_text(encoding="utf-8")
            header = source.split("\njobs:", 1)[0]
            if "pull_request" not in header:
                continue
            with self.subTest(workflow=path.name):
                self.assertIn(
                    "concurrency:",
                    header,
                    f"{path.name} runs on pull_request without superseded-run cancellation",
                )
                self.assertIn(
                    "cancel-in-progress:",
                    header,
                    f"{path.name} must declare cancel-in-progress",
                )

    def test_release_workflow_never_cancels_a_publish(self):
        release = (ROOT / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            release,
            "release.yml must cancel only pull-request runs, never a tag publish",
        )

    def test_draft_runs_do_not_consume_ci_and_never_wedge_a_pr(self):
        import re

        workflow_dir = ROOT / ".github" / "workflows"

        for path in sorted(workflow_dir.glob("*.yml")):
            source = path.read_text(encoding="utf-8")
            header = source.split("\njobs:", 1)[0]
            if "pull_request" not in header:
                continue
            if "github.event.pull_request.draft == false" not in source:
                continue
            with self.subTest(workflow=path.name):
                self.assertIn(
                    "ready_for_review",
                    header,
                    f"{path.name} skips draft runs but would not re-run when the PR is marked ready",
                )
                # Multi-trigger workflows must scope the guard to pull_request
                # events, otherwise push/schedule runs are skipped too.
                if re.search(r"^  (push|schedule|workflow_dispatch):", header, re.M):
                    self.assertIn(
                        "github.event_name != 'pull_request' ||",
                        source,
                        f"{path.name} also runs on non-PR events, so its draft guard must be event-scoped",
                    )

    def test_contract_is_enforced_by_the_implementation_gate(self):
        gate = (ROOT / "scripts" / "verify_implementation.py").read_text(encoding="utf-8")

        self.assertIn(
            "test_github_actions_ci_contract",
            gate,
            "the CI contract test must run from scripts/verify_implementation.py",
        )

    def test_retired_external_ci_configs_are_not_restored(self):
        for retired_path in (
            ".woodpecker/rust.yml",
            ".buildkite/pipeline.yml",
            "ci.yml",
            "scripts/ci/buildkite-rust-static.sh",
            "scripts/ci/buildkite-rust-tests.sh",
        ):
            with self.subTest(path=retired_path):
                self.assertFalse(
                    (ROOT / retired_path).exists(),
                    f"retired CI configuration reintroduced: {retired_path}",
                )


if __name__ == "__main__":
    unittest.main()
