# Docs-only PR classification (advisory pilot)

`python3 scripts/classify_pr_paths.py --base "$PR_BASE_SHA" --head "$PR_HEAD_SHA"`

The output is a JSON audit record containing the exact base/head commit IDs, their merge base, every changed path (including *both* names on a rename), and whether the change could be eligible for a fast documentation-only validation lane. A nonzero exit means the scope could not be established, so **run full CI**. A valid but empty diff, or a mix of documentation and code, also yields `mode: full`.

The allowlist intentionally covers Markdown/MDX under `docs/` and `RFC/`, the root `README.md` / `CONTRIBUTING.md` / `CHANGELOG.md`, and GitHub issue-form YAML. Changes to `.astro`, `.ts`, `.js`, `Cargo*`, Rust, generated artifacts, scripts, workflows, and unknown files require the full matrix. The classifier uses Git's NUL-delimited name/status diff, the merge base, and both commit trees. Renamed or deleted code cannot be hidden by a new documentation path, and symlinks, executable files, and submodules do not qualify as docs-only.

Locally validate this component with:

```bash
python3 -m unittest discover -s scripts/tests -p 'test_classify_pr_paths.py' -v
```

The draft also includes `scripts/validate_issue_forms.py`, a conservative GitHub issue-form YAML validator. It uses pinned PyYAML 6.0.3, rejects duplicate keys/unsafe YAML tags, checks required form fields/control IDs, and returns a nonzero exit on invalid forms. Local validation:

```bash
python3 -m pip install PyYAML==6.0.3
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
python3 scripts/validate_issue_forms.py
```

**Current CI scope:** `.github/workflows/ci.yml` installs PyYAML and runs the issue-form validation in its **existing Build & Test job**. No additional jobs, required contexts, or skip behavior are introduced. This is not yet a fast docs-only check; the complete CI matrix remains in force. The classifier remains advisory, and protected CI must still execute on the PR's exact head. Before using it to gate jobs:

1. Complete the required-check/provider migration (#1551) and confirm real Actions runner execution (#1431).
2. Inventory the exact protected job contexts; design a stable *required* aggregate that fails when docs validation fails and retains the full matrix for mixed/code changes.
3. Add fast docs validation (Markdown/MDX, issue-form YAML, internal links, Astro checks/build/navigation) with actionable red failures.
4. Prove terminal exact-head conclusions on separate docs-only and docs+Rust trial PRs, including one deliberately broken MDX/YAML change. Keep the full matrix until these tests and the protection migration pass.