# Nulang main branch required-check acceptance

This is a **read-only** verification procedure for security issue
[#1652](https://github.com/nulang-org/nulang/issues/1652).
It does not configure a ruleset, approve a pull request, or authorize merge.

## Operator prerequisites

- A GitHub organization/repository administrator with **authorized** access to
  Rulesets, and a **different, non-bypass** account to test merge-blocking rules.
- An independent security/code reviewer. Avoid making the PR author the only
  CODEOWNER; that will deadlock the independent-review requirement.
- Verify the current ruleset ID 25012292 and default branch on GitHub first.
  It was last observed active, on **all branches**, with 1 required review,
  stale approvals allowed, unresolved review threads allowed, **no required
  status checks**, and broad always-bypass actors.
- Read [#1551](https://github.com/nulang-org/nulang/issues/1551) before
  touching legacy Woodpecker/Buildkite contexts. GitHub Actions is the tested
  candidate; do not silently weaken or disable legacy required protections.

## Read-only validation

With Python 3.10+ and no additional dependencies:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_main_ruleset_audit.py' -v
python3 scripts/admin/audit_main_ruleset.py
```

The second command is **expected to fail** until the repository is protected.
It executes only a GitHub REST GET. If GitHub omits bypass actors in an
unauthenticated response, the audit fails closed. To obtain full rule details,
an administrator can provide a read-only `GITHUB_TOKEN` using a secure
credential manager; never print it or record it in an issue.

To inspect an independently saved complete GitHub ruleset response without
network access, use `--file /path/to/ruleset.json`. Do not copy tokens or
other credentials into the JSON.

## Required checks

These **exact check-run names** and source were read back from the completed
[Fabric/security exact-head PR #2278](https://github.com/nulang-org/nulang/pull/2278),
commit `48926bb098eefff3d96921c83cbf3836adf24b3d`.
The expected provider is **GitHub Actions (integration ID 15368)**:

- `Build & Test`
- `Build & Test (wasm-backend feature)`
- `Build without optional features`
- `Release Build`
- `Format, Clippy & Audit`
- `Docs site & browser playground`
- `Validate PR title`
- `Analyze (Rust) (rust)`

Before enforcing, use the actual GitHub Settings ruleset check picker and
validate each required job is emitted on every ordinary non-draft PR. The
rule should require checks against an up-to-date branch, not only any passing
historical status. **Do not require** `Publish Release`,
`Changelog updated?`, or `Benchmarks (regression-gated)`: those were
deliberately skipped for this PR.

## Administrator change and acceptance

1. Open GitHub **Settings → Rules → Rulesets → rules (25012292)**.
   **Preserve `~ALL` branch coverage** unless the repository administrator
   independently justifies a different scope and installs equivalent rules
   for other branches. `~ALL` already protects `main`; it is not a defect.
   The audit accepts `~ALL`, `~DEFAULT_BRANCH` or `refs/heads/main` as long as
   `main` is not excluded.
2. Add a `required_status_checks` rule for the stable job names above,
   selecting **GitHub Actions** as the producing app for each. Do not use
   `any source`, a path-filtered/check that is usually skipped, or old
   Woodpecker contexts without a verified active provider.
3. Keep PR review required and last-push approval enabled. Require a real
   non-author approval, dismiss stale approvals after pushes, and require
   resolving review threads. Configure CODEOWNERS only with a qualified
   independent owner who can review. Review broad always-bypass actors with
   the repo administrator; preserve only documented break-glass operations.
4. Read back the whole ruleset with sufficient access. Re-run the script
   and expect exit 0. The audit is **not** itself a GitHub required check.
5. Using a **non-bypass** actor, verify missing/failing exact-head status,
   missing independent review, and unresolved thread all block merge.
   Verify a fully green, freshly reviewed *safe test PR* can be merged through
   the ordinary route. Do not use #2278 as the sacrificial merge experiment.
6. Post the sanitized read-back, approved provider IDs, negative/positive
   acceptance evidence, and reviewer roster on issue #1652.

This checker intentionally detects policy drift but does not install or
change security controls. Passing it alone does **not** certify a Turso
database, replace an independent security assessment, or make #2278
merge-ready. Turso validation is tracked in
[#2284](https://github.com/nulang-org/nulang/issues/2284).
