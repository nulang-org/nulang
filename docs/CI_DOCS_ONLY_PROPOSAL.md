# Docs-only PR classification (advisory pilot)

`python3 scripts/classify_pr_paths.py --base "$PR_BASE_SHA" --head "$PR_HEAD_SHA"`

The output is a JSON audit record containing the exact base/head commit IDs, their merge base, every changed path (including *both* names on a rename), and whether the change could be eligible for a fast documentation-only validation lane. A nonzero exit means the scope could not be established, so **run full CI**. A valid but empty diff, or a mix of documentation and code, also yields `mode: full`.

The allowlist intentionally covers Markdown/MDX under `docs/` and `RFC/`, the root `README.md` / `CONTRIBUTING.md` / `CHANGELOG.md`, and GitHub issue-form YAML. Changes to `.astro`, `.ts`, `.js`, `Cargo*`, Rust, generated artifacts, scripts, workflows, and unknown files require the full matrix. The classifier uses Git's NUL-delimited name/status diff, the merge base, and both commit trees. Renamed or deleted code cannot be hidden by a new documentation path, and symlinks, executable files, and submodules do not qualify as docs-only.

Locally validate this component with:

```bash
python3 -m unittest discover -s scripts/tests -p 'test_classify_pr_paths.py' -v
```

**Important:** this commit does **not** change `.github/workflows/ci.yml`, skip any job, alter branch protection, or establish a replacement required check. The classification is only preparatory work for #1566. Before using it to gate jobs:

1. Complete the required-check/provider migration (#1551) and confirm real Actions runner execution (#1431).
2. Inventory the exact protected job contexts; design a stable *required* aggregate that fails when docs validation fails and retains the full matrix for mixed/code changes.
3. Add fast docs validation (Markdown/MDX, issue-form YAML, internal links, Astro checks/build/navigation) with actionable red failures.
4. Prove terminal exact-head conclusions on separate docs-only and docs+Rust trial PRs, including one deliberately broken MDX/YAML change. Keep the full matrix until these tests and the protection migration pass.