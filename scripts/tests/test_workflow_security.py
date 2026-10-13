"""Reject direct untrusted GitHub event interpolation in workflow shell scripts.

GitHub Actions evaluates expressions in 'run' prior to shell execution.
Contributor-controlled metadata belongs in 'env:' or action inputs, never in
interpolated script source.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
RUN_KEY = re.compile(r"^(\s*)(?:-\s*)?run:\s*(.*)$")
UNTRUSTED_EXPRESSION = re.compile(
    r"\$\{\{\s*(?:"
    r"github\.event\.(?:pull_request|issue|comment|review|review_comment|"
    r"head_commit|commits|discussion)(?:\.|\[)"
    r"|github\.head_ref\b)"
)


def untrusted_run_lines(source: str) -> list[int]:
    """Report source line numbers with untrusted inline GitHub expressions."""
    lines = source.splitlines()
    findings = []
    idx = 0
    while idx < len(lines):
        match = RUN_KEY.match(lines[idx])
        if not match:
            idx += 1
            continue
        indent = len(match.group(1))
        inline = match.group(2)
        if UNTRUSTED_EXPRESSION.search(inline):
            findings.append(idx + 1)
        idx += 1
        if inline.startswith(("|", ">")):
            while idx < len(lines):
                current = lines[idx]
                if current.strip() and len(current) - len(current.lstrip()) <= indent:
                    break
                if UNTRUSTED_EXPRESSION.search(current):
                    findings.append(idx + 1)
                idx += 1
    return findings


class WorkflowSecurityTests(unittest.TestCase):
    def test_rejects_multiline_script_interpolation(self):
        source = 'steps:\n  - run: |\n      title="${{ github.event.pull_request.title }}"\n'
        self.assertEqual(untrusted_run_lines(source), [3])

    def test_rejects_single_line_run_interpolation(self):
        source = 'steps:\n  - run: echo "${{ github.head_ref }}"\n'
        self.assertEqual(untrusted_run_lines(source), [2])

    def test_env_binding_is_not_shell_source(self):
        source = (
            'steps:\n  - env:\n      PR_TITLE: ${{ github.event.pull_request.title }}\n'
            '    run: |\n      printf "%s\\n" "$PR_TITLE"\n'
        )
        self.assertEqual(untrusted_run_lines(source), [])

    def test_static_github_expression_is_allowed(self):
        self.assertEqual(
            untrusted_run_lines('steps:\n  - run: echo "${{ github.sha }}"\n'), []
        )

    def test_repository_workflows_do_not_inline_untrusted_event_data(self):
        violations = []
        for path in sorted([*WORKFLOWS.glob("*.yml"), *WORKFLOWS.glob("*.yaml")]):
            for lineno in untrusted_run_lines(path.read_text(encoding="utf-8")):
                violations.append(f"{path.relative_to(ROOT)}:{lineno}")
        self.assertEqual(
            violations, [],
            "Use env: for untrusted GitHub values: " + ", ".join(violations)
        )


if __name__ == "__main__":
    unittest.main()
