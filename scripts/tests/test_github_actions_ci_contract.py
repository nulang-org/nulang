"""Guard the GitHub Actions CI admission and validation contract."""

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW_DIR = ROOT / ".github" / "workflows"
CI = WORKFLOW_DIR / "ci.yml"
RELEASE = WORKFLOW_DIR / "release.yml"
PR_WORKFLOWS = (
    "actor-density-ab.yml",
    "capacity.yml",
    "changelog.yml",
    "ci.yml",
    "cluster-gossip-throttle-pr.yml",
    "codeql.yml",
    "cross-runtime-bench.yml",
    "jit-codegen-bench.yml",
    "nulang-ab-bench.yml",
    "pr-title.yml",
    "release.yml",
    "remote-rtt-stage-profile-pr.yml",
    "shard-scaling-bench.yml",
    "vscode-extension.yml",
)


class GitHubActionsCIContractTests(unittest.TestCase):
    def test_primary_ci_keeps_required_contexts_and_full_validation(self):
        source = CI.read_text(encoding="utf-8")

        for context in (
            "Release Build",
            "Nula Package Test",
            "Build without optional features",
            "Build & Test (wasm-backend feature)",
            "Format, Clippy & Audit",
            "Lean Formalization",
            "Build & Test",
        ):
            with self.subTest(context=context):
                self.assertIn(f"name: {context}", source)

        for command in (
            "cargo nextest run --locked",
            "cargo nextest run --locked --release --no-default-features --features release-cli",
            "cargo nextest run --locked --no-default-features",
            "cargo nextest run --locked --features wasm-backend",
            "cargo test --locked --doc",
            "cargo test --locked --release --no-default-features --features release-cli --doc",
            "cargo test --locked --no-default-features --doc",
            "cargo test --locked --features wasm-backend --doc",
            "cargo fmt --check",
            "cargo clippy --locked --all-targets",
            "cargo audit",
            "git diff --check",
        ):
            with self.subTest(command=command):
                self.assertIn(command, source)

    def test_pull_request_workflows_skip_drafts_and_resume_when_ready(self):
        for filename in PR_WORKFLOWS:
            source = (WORKFLOW_DIR / filename).read_text(encoding="utf-8")
            with self.subTest(workflow=filename):
                self.assertIn("ready_for_review", source)
                self.assertIn("converted_to_draft", source)
                self.assertIn(
                    "github.event.pull_request.draft == false",
                    source,
                )
                self.assertIn("concurrency:", source)
                self.assertIn("cancel-in-progress:", source)

        ci = CI.read_text(encoding="utf-8")
        release = RELEASE.read_text(encoding="utf-8")
        self.assertIn("needs.release.result == 'success'", ci)
        self.assertIn("needs.preflight.result == 'success'", release)
        self.assertIn("fetch-depth: 0", ci)
        self.assertIn(
            'git diff --check "$PR_BASE_SHA...$PR_HEAD_SHA"',
            ci,
        )
        release_trigger = release.split("  pull_request:\n", 1)[1].split(
            "    paths:\n", 1
        )[0]
        self.assertNotIn("labeled", release_trigger)

    def test_release_concurrency_never_cancels_tagged_publish(self):
        source = RELEASE.read_text(encoding="utf-8")
        self.assertIn(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            source,
        )
        self.assertIn("if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')", source)


if __name__ == "__main__":
    unittest.main()
