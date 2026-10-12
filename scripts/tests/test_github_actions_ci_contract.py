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

        self.assertIn("cargo nextest run --locked", source)
        self.assertIn("cargo nextest run --locked --features wasm-backend", source)
        self.assertIn("cargo test --locked --no-default-features", source)
        self.assertIn("cargo clippy --locked --all-targets", source)

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
