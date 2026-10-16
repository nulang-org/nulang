"""Fail-closed PR change classification regression tests."""

import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "classify_pr_paths.py"
spec = importlib.util.spec_from_file_location("classify_pr_paths", SCRIPT)
classifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(classifier)


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.PIPE).decode().strip()


class PrPathClassifierTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = pathlib.Path(self.tmp.name)
        git(self.repo, "init", "-b", "main")
        git(self.repo, "config", "user.name", "Test")
        git(self.repo, "config", "user.email", "test@example.com")
        self.write("docs/intro.md", "# Guide\n")
        self.write("src/compiler.rs", "fn base() {}\n")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-m", "initial")
        self.base = git(self.repo, "rev-parse", "HEAD")
        git(self.repo, "checkout", "-b", "feature")

    def write(self, name, text):
        target = self.repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self):
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-m", "change")
        return git(self.repo, "rev-parse", "HEAD")

    def classify(self, head):
        return classifier.classify(self.repo, self.base, head)

    def test_markdown_mdx_and_issue_form_changes_are_docs_only(self):
        self.write("docs/intro.md", "# Updated Guide\n")
        self.write("docs/guide.mdx", "# MDX\n")
        self.write(".github/ISSUE_TEMPLATE/bug_report.yml", "name: Bug\n")
        self.write("README.md", "# Readme\n")
        head = self.commit()
        result = self.classify(head)
        self.assertEqual("docs-only", result["mode"])
        self.assertEqual(head, result["head"])
        self.assertIn("docs/guide.mdx", result["paths"])

    def test_docs_plus_rust_is_full(self):
        self.write("docs/intro.md", "# Updated Guide\n")
        self.write("src/compiler.rs", "fn changed() {}\n")
        result = self.classify(self.commit())
        self.assertEqual("full", result["mode"])
        self.assertIn("src/compiler.rs", result["unsafe_paths"])

    def test_docs_site_source_workflow_and_cargo_changes_are_full(self):
        for filename in ("docs/src/pages/index.astro", ".github/workflows/ci.yml", "Cargo.lock", "docs/config.yml"):
            with self.subTest(filename=filename):
                git(self.repo, "reset", "--hard", self.base)
                self.write(filename, "changed\n")
                head = self.commit()
                self.assertEqual("full", self.classify(head)["mode"])

    def test_rename_from_rust_into_docs_fails_closed(self):
        git(self.repo, "mv", "src/compiler.rs", "docs/compiler.md")
        result = self.classify(self.commit())
        self.assertEqual("full", result["mode"])
        self.assertIn("src/compiler.rs", result["unsafe_paths"])

    def test_docs_rename_stays_docs_only(self):
        git(self.repo, "mv", "docs/intro.md", "docs/renamed.md")
        result = self.classify(self.commit())
        self.assertEqual("docs-only", result["mode"])
        self.assertEqual(["docs/intro.md", "docs/renamed.md"], result["paths"])

    def test_new_docs_symlink_does_not_qualify(self):
        (self.repo / "docs" / "linked.md").symlink_to("intro.md")
        self.assertEqual("full", self.classify(self.commit())["mode"])

    def test_executable_docs_change_does_not_qualify(self):
        (self.repo / "docs" / "intro.md").chmod(0o755)
        self.assertEqual("full", self.classify(self.commit())["mode"])

    def test_deleting_code_is_full(self):
        git(self.repo, "rm", "src/compiler.rs")
        result = self.classify(self.commit())
        self.assertEqual("full", result["mode"])

    def test_no_changed_files_is_not_docs_only(self):
        self.assertEqual("full", self.classify(self.base)["mode"])

    def test_comparison_uses_merge_base_not_destination_tip(self):
        self.write("docs/intro.md", "# Feature\n")
        head = self.commit()
        git(self.repo, "checkout", "main")
        self.write("src/compiler.rs", "fn updated_on_main() {}\n")
        new_main = self.commit()
        result = classifier.classify(self.repo, new_main, head)
        self.assertEqual("docs-only", result["mode"])
        self.assertEqual(self.base, result["merge_base"])

    def test_docs_only_status_is_emitted_by_cli_for_exact_shas(self):
        self.write("docs/intro.md", "# Changed\n")
        head = self.commit()
        output = subprocess.check_output([
            "python3", str(SCRIPT), "--repo", str(self.repo),
            "--base", self.base, "--head", head,
        ], text=True)
        self.assertEqual("docs-only", json.loads(output)["mode"])

    def test_invalid_base_fails_closed_with_error(self):
        self.write("docs/intro.md", "# Changed\n")
        head = self.commit()
        with self.assertRaises(classifier.ClassificationError):
            classifier.classify(self.repo, "does-not-exist", head)

    def test_unknown_status_and_malformed_nul_output_rejected(self):
        with self.assertRaises(classifier.ClassificationError):
            classifier.parse_name_status(b"R100\0old.md\0")
        with self.assertRaises(classifier.ClassificationError):
            classifier.parse_name_status(b"Z\0README.md\0")

    def test_paths_with_tabs_and_newlines_are_nul_safe(self):
        filename = "docs/a\tline\n.md"
        self.write(filename, "# Heading\n")
        result = self.classify(self.commit())
        self.assertEqual("docs-only", result["mode"])
        self.assertIn(filename, result["paths"])


if __name__ == "__main__":
    unittest.main()
