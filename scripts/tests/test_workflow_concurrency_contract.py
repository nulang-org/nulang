"""Contract tests: superseded PR diagnostics must not clog Actions runners.

The release tag publish path intentionally has different cancellation semantics
from pull-request packaging validation.
"""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"


class WorkflowConcurrencyContractTests(unittest.TestCase):
    def test_remote_rtt_cancels_superseded_pull_request_runs(self):
        text = (WORKFLOWS / "remote-rtt-stage-profile-pr.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("  pull_request:\n", text)
        self.assertIn(
            "  group: remote-rtt-${{ github.event.pull_request.number || github.ref }}",
            text,
        )
        self.assertIn("  cancel-in-progress: true", text)
        self.assertIn("- name: Release stage budget", text)

    def test_release_cancels_pr_only_never_release_tag(self):
        text = (WORKFLOWS / "release.yml").read_text(encoding="utf-8")
        self.assertIn("    tags: ['v*']", text)
        self.assertIn("  pull_request:\n", text)
        self.assertIn(
            "  group: release-${{ github.event_name }}-${{ github.event.pull_request.number || github.ref }}",
            text,
        )
        self.assertIn(
            "  cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            text,
        )
        self.assertIn(
            "    if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')",
            text,
        )


if __name__ == "__main__":
    unittest.main()
