# Nulang roadmap

> **Updated:** 2026-10-12 · **Status:** contributor-facing priorities, not a release schedule.
>
> Nulang is alpha software. The work below is **planned or in progress, not
> shipped** merely because it appears here. Priorities can change when tests,
> production evidence, or user feedback expose a more serious problem.

This is the short entry point for contributors. [Open issues](https://github.com/nulang-org/nulang/issues)
hold actionable tasks and current assignments; [PRs](https://github.com/nulang-org/nulang/pulls)
show proposed changes. [docs/IMPLEMENTATION_STATUS.md](docs/IMPLEMENTATION_STATUS.md)
and executable tests describe implemented behavior. [GOVERNANCE.md](GOVERNANCE.md)
and accepted [RFCs](RFC/) determine what changes are permitted.
The detailed [production planning draft](docs/PLAN.md) is a historical planning
artifact, **not** a live issue queue or an alternative acceptance policy.

## Now: correctness and a trustworthy contribution path (P0)

| Priority | Outcome / acceptance direction | Live tracking |
| --- | --- | --- |
| P0 | Exact-head GitHub Actions checks make regressions visible and reliable; no promotion on unverified CI. | [CI transition #1551](https://github.com/nulang-org/nulang/issues/1551), [stabilization gate #231](https://github.com/nulang-org/nulang/issues/231) |
| P0 | Preserve ownership, authority, identity, recovery, and replay invariants across compiler/runtime boundaries; fail closed rather than guessing. | [Stabilization #231](https://github.com/nulang-org/nulang/issues/231), [architecture #331](https://github.com/nulang-org/nulang/issues/331) |
| P0 | Make durable workflow transitions and actor delivery/recovery semantics explicit, crash-testable, and observable. | [Workflow state #1548](https://github.com/nulang-org/nulang/issues/1548), [delivery #1180](https://github.com/nulang-org/nulang/issues/1180), [replay #836](https://github.com/nulang-org/nulang/issues/836) |

This sequence follows the active
[semantic stabilization contract](docs/SEMANTIC_STABILIZATION_CONTRACT.md).
Large new language features are **not** a substitute for proving existing
runtime guarantees.

## Next: adoption, tooling, and interoperability (P1)

| Area | Useful contribution | Live tracking |
| --- | --- | --- |
| First-user experience | Reproduce install or quick-start failures, improve diagnostics and docs with runnable examples, and validate an independent adoption pilot. | [Adoption gate #809](https://github.com/nulang-org/nulang/issues/809) |
| Compiler and editor DX | Improve parser/formatter/spec consistency *after* the stabilization gate and add narrow regression fixtures. | [Syntax DX #630](https://github.com/nulang-org/nulang/issues/630) |
| Portability | Remove accidental runtime dependencies from release artifacts and test installation paths. | [Release portability #1472](https://github.com/nulang-org/nulang/issues/1472) |
| Build footprint | Reduce unnecessary default dependencies without weakening language semantics or feature-matrix CI. | [Build profiles #1048](https://github.com/nulang-org/nulang/issues/1048) |

Documentation, small reproducible bugs, test cases, editor tooling, and
examples are welcome even when the underlying runtime work is complex.

## Later: scale and broader platform capabilities (research / experimental)

- Improve multi-node failure handling, mailbox bounds, scheduling and
  performance with measurable repeatable tests
  ([backpressure #456](https://github.com/nulang-org/nulang/issues/456),
  [actor density #284](https://github.com/nulang-org/nulang/issues/284)).
- Expand portable backend and component interoperability only as equivalent
  semantics and conformance coverage are established.
- Mature Experimental Fabric, workflow, and data components through documented
  failure models; do **not** describe them as production replacements for
  external infrastructure before the guarantees are demonstrated.

These are directions, **not** commitments to delivery dates or compatibility
claims. Performance work requires matched-runner measurements.

## How to help

1. Read [CONTRIBUTING.md](CONTRIBUTING.md) and choose a small issue with a
   reproducible problem and a clear acceptance test. Start with
   [curated good-first issues](https://github.com/nulang-org/nulang/issues?q=is%3Aissue+is%3Aopen+label%3Agood-first-issue) or
   [all open issues](https://github.com/nulang-org/nulang/issues).
   Two scoped documentation tasks were filed on 2026-10-12:
   [string-literal wording #1562](https://github.com/nulang-org/nulang/issues/1562)
   and [source-build quick start #1563](https://github.com/nulang-org/nulang/issues/1563).
   The quick-start fix is already proposed in this README/onboarding PR,
   so **#1563 is not available for a new contributor to claim**. Check each
   issue's current state and assignments before starting work.
2. If no suitable issue exists, [open a discussion](https://github.com/nulang-org/nulang/discussions)
   with the problem and your proposed scope, or file an
   [issue](https://github.com/nulang-org/nulang/issues/new/choose).
3. For any language semantics / Frozen or Stable surface change, first follow
   [governance and the RFC process](GOVERNANCE.md). An issue being on the
   roadmap is not RFC approval.
4. Include focused tests, expected versus actual behavior, and a small PR.
   Maintainers use the [review/triage playbook](.github/CONTRIBUTOR_REVIEW_GUIDE.md)
   for scope and acceptance.

**Roadmap maintenance:** Review this page against the issue queue when a
priority or milestone changes. Close or retarget completed/stale tracker links;
do not mark work done until it is merged and validated. The issue's current
status and the exact checked commit override an older snapshot here.
