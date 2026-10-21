# Contributor issue triage and PR review playbook

This page is for maintainers and volunteer reviewers. It complements
[CONTRIBUTING.md](../CONTRIBUTING.md) and [GOVERNANCE.md](../GOVERNANCE.md),
not a promise of response times or unconditional acceptance.

## Triage new issues

1. **Check safety:** move vulnerability details to the private process in
   [SECURITY.md](../SECURITY.md); never solicit public exploit details.
2. **Deduplicate and classify:** link existing issues/PRs; determine whether
   the report is a bug, feature request, docs issue, or support question.
3. **Request evidence only if needed:** exact version/commit, platform,
   feature flags, smallest reproduction, observed vs expected results, and
   relevant logs with secrets removed. For a spelling or navigation fix,
   a page URL and correction are enough; do **not** demand a compiler build
   or reproduction environment.
4. **Decide priority and scope:** correctness/security/compatibility and the
   active [stabilization gate](../docs/SEMANTIC_STABILIZATION_CONTRACT.md) come
   before speculative surface expansion. Record what will be accepted and the
   smallest automated verification.
5. **Make work claimable:** split broad requests into independent outcomes;
   link upstream blocker and RFC; note likely touched files and tests. Invite
   a contributor to comment before starting substantial work.

Labels are discovery aids, **not** proof that an issue is staffed, unblocked,
or approved for implementation. Apply `good-first-issue` only when the issue
has a self-contained reproduction or doc target, clear acceptance criteria,
no unresolved language-design decision, and limited setup. Apply
`help-wanted` only when external implementation is genuinely welcome;
unlabel stale/blocked work. The queue may initially be empty. Keep
`roadmap` priority tags synchronized with the roadmap; do not use roadmap
labels to waive the RFC process.

A good task description answers:

- **Problem and current evidence** (link to failing test, observed behavior,
  or a precise doc location).
- **Expected result** with a concrete acceptance test.
- **Scope boundaries**: likely files, out-of-scope changes, dependencies,
  compatibility/RFC constraints.
- **Validation**: command(s), test fixture, or deterministic doc verification.
- **Ownership:** whether work is already being attempted in a PR.

## Review incoming PRs

1. Confirm scope and issue fit. If it changes Frozen/Stable language semantics,
   require an **accepted RFC first**; an open issue is not authorization.
2. Prefer a minimal, reviewable patch. Reject unrelated formatting sweeps,
   unexamined dependency churn, and unsupported marketing/compatibility claims.
3. Inspect proof: prior failure, new regression, feature-matrix boundaries,
   expected error paths, affected reference semantics, and changelog/docs.
4. Check CI on the **current PR head SHA**, not a green run from a previous
   commit. Distinguish infrastructure blockage from a proven code failure.
5. Offer specific requested changes, with file/line references and test
   expectations. Avoid requiring full-suite local execution for docs-only
   edits; use risk-adjusted checks.
6. When checks are queued, cancelled or skipped, distinguish an unexecuted
   lane from a code failure. Link the existing queue incident (currently
   [#1431](https://github.com/nulang-org/nulang/issues/1431)) rather than
   rerunning many jobs or treating deployment success as sufficient evidence.
7. Before merge, verify requested review changes, CI, no new regressions,
   and a clear compatibility decision. The project steward retains final
   authority as defined in governance.

## Keeping the queue healthy

Periodically identify a few *verified* beginner tasks from reproducible
docs, tests, diagnostics, or examples. Avoid fake `good-first-issue` labels on
P0 semantics work. Close duplicates with a link and rationale. For stale tasks,
confirm the current code state before closing; don't auto-close assigned or
security-critical work based on age alone. When a milestone changes, update
[ROADMAP.md](../ROADMAP.md) and the tracked issue together.
