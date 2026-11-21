# Maintainer Review and Merge Policy

This policy supplements [CONTRIBUTING.md](../CONTRIBUTING.md),
[GOVERNANCE.md](../GOVERNANCE.md), and [SECURITY.md](../SECURITY.md).
It applies equally to maintainers, bots, and first-time contributors.
A green check means only that the specific check ran successfully; it
does **not** prove semantic correctness or an absence of vulnerabilities.

## Admission rules

1. Merge through a pull request, not an unreviewed direct push to `main`.
   A pull request author must not be the sole reviewer of their own change.
2. Review the **entire diff against the actual target base**, including
   transitive stacked-PR changes. A green result on a parent or old head does
   not validate a changed merge candidate.
3. Require relevant tests with assertions for expected behavior **and**
   failure behavior. Reproduction tests precede bug fixes. If a test could
   not execute, mark it unverified and do not describe it as passing.
4. Do not weaken/skip failing tests, format checks, lint, safety checks,
   changelog obligations, or security scans to make a PR green. A documented,
   narrowly scoped exception needs an explicit maintainer decision.
5. Semantic/format/protocol changes follow the stability tiers and RFC
   process in `GOVERNANCE.md`. Keep conformance, formal artifacts,
   diagnostics, versioning, migrations, and `CHANGELOG.md` consistent.
6. New performance claims require reproducible, same-host, exact-revision
   measurements; compare identical workloads with correctness assertions.
   Shared-runner timing is diagnostic, not release evidence.
7. Keep changes small and explain dependencies. Stacked PRs must land in order
   and must be revalidated against the new base after each merge. Do not
   accept an untested feature behind a flag as if the flag guarantees safety.

## High-risk changes require security-specific review

Treat these as high risk: bytecode parsing/verification, value layout, JIT/
native code generation, unsafe Rust/FFI, package or registry provenance,
actor capability isolation, TLS/transport, durable journals/transactions,
serialization, external effects, sandboxing, and CI/release credentials.

Reviewers should inspect:

- **Threat model:** who controls each byte/identifier, and where trusted
  code crosses the boundary; distinguish malformed from merely unexpected data
- **Resource limits:** lengths, counts, recursion, decompression ratios,
  elapsed time, memory use, and cleanup after partial failures
- **Correctness:** overflow/underflow, integer width, aliasing, captures,
  cancellation, retries, duplicate delivery, crash windows, and recovery
- **Authorization:** identity, tenant or actor scope, capability checks,
  signed provenance, replay protection and fail-closed behavior
- **Coverage:** malformed input, boundary values, differential backends,
  process restart/crash recovery, and negative authorization cases

An independent trusted security-aware maintainer should review high-risk
changes once that reviewer is available. Until then, keep high-risk changes
in draft rather than converting an unreviewed self-authored PR into a merge.

## CI and dependency trust boundaries

- Use `pull_request` (not `pull_request_target` plus a checkout of
  contributor code) for code that comes from forks.
- Give PR jobs `contents: read` and no write scopes/secrets. Keep publishing,
  signed releases and benchmark-history writes in separate privileged jobs
  gated to trusted refs and environments.
- Pass contributor-controlled GitHub event fields through action inputs or
  `env:` and quote shell variables. Never splice them into `run:` script
  source. The workflow-security regression check enforces common patterns.
- Review added GitHub Actions, dependency upgrades and generated lockfile
  changes. Prefer reviewed full-commit action pins where operationally
  practical; do not execute freshly downloaded scripts with broad tokens.
- Treat `cargo audit` and CodeQL as **partial signals**. Review known advisory
  suppressions in `.cargo/audit.toml`; Rust CodeQL does not cover the
  TypeScript registry worker or the Python playground server.
- No automatic merging of security-related dependency changes solely
  because the dependency bot opened the PR.

## Repository ruleset to configure in GitHub

The files in this PR cannot enable server-side branch protection. At audit
time (2026-10-13), repository ruleset [#25012292](https://github.com/nulang-org/nulang/rules/25012292)
was **active and targeted all branches**. It already required one approval,
code-owner approval, and approval after the last push, but had **no required
status checks**, did not dismiss stale approvals, did not require conversations
resolved, and had broad always-bypass actors. The branch-protection endpoint
was not accessible through the installed GitHub connection. An administrator
must edit this existing ruleset (rather than create a conflicting one)
to protect `main` with these requirements:

1. Require pull requests; block force pushes and branch deletion.
2. Keep at least **one independent approval** and last-push approval;
   dismiss stale approvals after changes and require conversations resolved.
   The live ruleset already enables code-owner review; **do not introduce**
   a sole-owner `CODEOWNERS` file. Add code owners only after a second
   trusted maintainer can review the primary maintainer's PRs.
3. Require up-to-date passing checks on the merge candidate, including
   `CI / Build & Test`, `CI / Format, Clippy & Audit`,
   `CI / Build & Test (wasm-backend feature)`,
   `CI / Build without optional features`,
   `CI / Release Build`,
   `PR Title Check / Validate PR title`, and
   `PR Title Check / Workflow script injection guard`.
   Validate the **actual names** in the GitHub ruleset picker.
4. Include `CI / Docs site & browser playground` and
   `CI / Nula Package Test` when operationally stable. Require the changelog
   job only after confirming how skipped/draft checks are evaluated.
   Do not make path-filtered CodeQL or main-only benchmark checks required
   for PRs on which they do not run.
5. Disable bypass for ordinary maintainers, block direct pushes, use a merge
   queue if throughput warrants it, and keep release credentials isolated.
   Do not enable a setting that prevents the only maintainer from recovering
   the repository before a second trusted maintainer exists.

Ruleset settings were **read and verified**, but cannot be changed by this GitHub connector. No ruleset changes were made by this PR.
