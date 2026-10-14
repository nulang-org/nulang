"""Static safety contract for generated-docs CI publication.

The repository disallows bot branch pushes, including force-pushes. Keep
successful generated output recoverable without weakening branch protection.
"""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/docs-sync.yml"


class DocsSyncPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_has_no_direct_branch_push_or_automated_pr_mutation(self):
        self.assertNotRegex(self.workflow, r"\bgit push\b|\bgh pr (?:create|edit)\b")
        self.assertIn("contents: read", self.workflow)
        self.assertNotIn("contents: write", self.workflow)

    def test_archives_all_generated_assets_with_strict_missing_file_guard(self):
        self.assertIn("actions/upload-artifact@v7", self.workflow)
        self.assertIn("name: nulang-generated-docs", self.workflow)
        for path in (
            "docs/src/content/docs/stdlib/",
            "docs/api.md",
            "docs/public/playground/",
        ):
            self.assertIn(path, self.workflow)
        self.assertIn("if-no-files-found: error", self.workflow)
        self.assertIn("retention-days: 30", self.workflow)

    def test_diff_is_reported_explicitly_without_hiding_generation_failures(self):
        self.assertIn("node scripts/generate-docs.js", self.workflow)
        self.assertIn("playground/web/build.sh", self.workflow)
        self.assertIn("pnpm run build", self.workflow)
        self.assertIn("if git diff --cached --quiet", self.workflow)
        self.assertIn("::warning::Generated docs differ from main", self.workflow)
        self.assertIn("      - name: Ping DeepWiki to refresh index", self.workflow)


if __name__ == "__main__":
    unittest.main()
