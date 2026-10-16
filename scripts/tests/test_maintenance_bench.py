"""Deterministic tests for comparable, revision-pinned maintenance evidence."""

import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "maintenance_bench.py"
spec = importlib.util.spec_from_file_location("maintenance_bench", SCRIPT)
maintenance_bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(maintenance_bench)


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def commit(repo, files, message):
    for name, content in files.items():
        path = repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if content is None:
            path.unlink()
        elif isinstance(content, bytes):
            path.write_bytes(content)
        else:
            path.write_text(content, encoding="utf-8")
    git(repo, "add", "-A")
    subprocess.run(["git", "-C", str(repo), "commit", "-qm", message], check=True)
    return git(repo, "rev-parse", "HEAD")


class MaintenanceBenchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        git(self.repo, "init", "-q")
        git(self.repo, "config", "user.email", "tester@example.invalid")
        git(self.repo, "config", "user.name", "Maintenance Bench")
        self.base = commit(
            self.repo,
            {"src/app.nula": "let a = 1\n", "Nulang.toml": "[package]\nname = 'app'\n"},
            "baseline",
        )

    def implementation(self, head, language="nulang", **updates):
        return {
            "language": language,
            "repo": "repo",
            "base_sha": self.base,
            "head_sha": head,
            "verification": {
                "status": "passed",
                "command": "nulang --check src/app.nula",
                "evidence": "test-log.txt",
            },
            **updates,
        }

    def manifest(self, implementations):
        path = self.root / "experiment.json"
        path.write_text(json.dumps({
            "schema_version": 1,
            "scenario": "durable-order",
            "task_id": "add-retry",
            "implementations": implementations,
        }), encoding="utf-8")
        return path

    def test_counts_revised_files_and_dependency_manifests(self):
        head = commit(self.repo, {
            "src/app.nula": "let a = 1\nlet b = 2\n",
            "Nulang.toml": "[package]\nname = 'app'\nversion = '0.2'\n",
            "with space.txt": "some text\n",
        }, "add feature")
        result = maintenance_bench.evaluate(self.manifest([self.implementation(head)]))
        impl = result["implementations"][0]
        self.assertEqual(3, impl["metrics"]["files_changed"])
        self.assertEqual(3, impl["metrics"]["lines_added"])
        self.assertEqual(0, impl["metrics"]["lines_deleted"])
        self.assertEqual(["Nulang.toml"], impl["metrics"]["dependency_manifests_changed"])
        self.assertEqual([], impl["metrics"]["binary_files"])
        self.assertEqual("incomplete", result["comparison_status"])

    def test_rename_and_binary_changes_do_not_inflate_changed_line_counts(self):
        binary = b"\x00\x01\x02"
        first = commit(self.repo, {"data.bin": binary}, "add binary")
        # Use a baseline containing the binary so modification is in scope.
        (self.repo / "src" / "app.nula").rename(self.repo / "src" / "renamed.nula")
        head = commit(self.repo, {"data.bin": b"\x00\x03\x04"}, "rename and edit binary")
        metrics = maintenance_bench.measure_change(self.repo, first, head)
        self.assertEqual(2, metrics["files_changed"])
        self.assertEqual(0, metrics["lines_added"])
        self.assertEqual(0, metrics["lines_deleted"])
        self.assertEqual(["data.bin"], metrics["binary_files"])

    def test_rejects_duplicate_languages_and_unpinned_refs(self):
        head = commit(self.repo, {"src/app.nula": "let a = 2\n"}, "change")
        with self.assertRaisesRegex(ValueError, "duplicate language"):
            maintenance_bench.evaluate(self.manifest([
                self.implementation(head), self.implementation(head),
            ]))
        with self.assertRaisesRegex(ValueError, "full 40-character"):
            maintenance_bench.evaluate(self.manifest([
                self.implementation(head, head_sha="main"),
            ]))

    def test_rejects_missing_revisions_and_missing_evidence(self):
        head = "0" * 40
        with self.assertRaisesRegex(ValueError, "not a commit"):
            maintenance_bench.evaluate(self.manifest([self.implementation(head)]))
        head = commit(self.repo, {"src/app.nula": "let a = 2\n"}, "change")
        with self.assertRaisesRegex(ValueError, "evidence"):
            maintenance_bench.evaluate(self.manifest([self.implementation(
                head, verification={"status": "passed", "command": "test", "evidence": ""}
            )]))

    def test_marks_comparison_ready_only_when_all_three_verified(self):
        head = commit(self.repo, {"src/app.nula": "let a = 3\n"}, "change")
        entries = [self.implementation(head, language=lang) for lang in ("nulang", "rust", "go")]
        result = maintenance_bench.evaluate(self.manifest(entries))
        self.assertEqual("ready_for_human_parity_review", result["comparison_status"])
        entries[1]["verification"]["status"] = "not_run"
        result = maintenance_bench.evaluate(self.manifest(entries))
        self.assertEqual("incomplete", result["comparison_status"])

    def test_reports_git_failure_as_error_not_empty_metrics(self):
        head = commit(self.repo, {"src/app.nula": "let a = 3\n"}, "change")
        with self.assertRaisesRegex(ValueError, "repository"):
            maintenance_bench.evaluate(self.manifest([
                self.implementation(head, repo="does-not-exist"),
            ]))

    def test_awkward_file_names_do_not_break_null_delimited_diff(self):
        head = commit(self.repo, {
            "contains\ttab.txt": "one\n",
            "contains\nnewline.txt": "two\n",
        }, "add awkward names")
        metrics = maintenance_bench.measure_change(self.repo, self.base, head)
        self.assertEqual(2, metrics["files_changed"])
        self.assertEqual(2, metrics["lines_added"])

    def test_cli_reports_invalid_input_without_creating_output(self):
        head = commit(self.repo, {"src/app.nula": "let a = 2\n"}, "change")
        manifest = self.manifest([self.implementation(head, head_sha="main")])
        output = self.root / "result.json"
        self.assertEqual(2, maintenance_bench.main([str(manifest), "--output", str(output)]))
        self.assertFalse(output.exists())
        self.manifest([self.implementation(head)])
        self.assertEqual(0, maintenance_bench.main([str(manifest), "--output", str(output)]))
        self.assertEqual("incomplete", json.loads(output.read_text())["comparison_status"])

    def test_does_not_include_uncommitted_changes(self):
        head = commit(self.repo, {"src/app.nula": "let a = 3\n"}, "change")
        (self.repo / "src/app.nula").write_text("uncommitted\n", encoding="utf-8")
        metrics = maintenance_bench.measure_change(self.repo, self.base, head)
        self.assertEqual(1, metrics["files_changed"])
        self.assertEqual(1, metrics["lines_added"])
        self.assertEqual(1, metrics["lines_deleted"])


if __name__ == "__main__":
    unittest.main()
