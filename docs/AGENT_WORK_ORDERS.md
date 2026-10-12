# Agent work orders and evidence

Use this contract for coding-agent implementation, debugging, performance work, and significant refactors. It supplements existing repository security, architecture, contribution, and CI rules; it never overrides them. A work order is an execution specification, not permission to release, deploy, change credentials, or bypass review.

## Minimum work-order contract

Record the following *before* starting implementation (as an issue, prompt file, or task handoff):

```yaml
id: ISSUE-OR-TASK-ID
goal: "User-visible or engineering outcome, not a predetermined fix"
evidence: "Observed failure, issue link, benchmark, or baseline; state if unverified"
repository: owner/repo
base_sha: "Full immutable commit SHA, not just main"
owns: ["path/or/subsystem/**"]
depends_on: []
allowed_actions: ["read", "edit", "test", "commit"]
non_goals: ["Unrelated refactoring", "Unrequested deploy or release"]
constraints:
  - "Preserve existing security and public compatibility contracts"
  - "Do not disable assertions, CI checks, or isolation to make tests pass"
acceptance:
  - "Specific observable behavior or regression test"
  - "Named verification commands and their expected result"
verification:
  focused: "Exact focused test or verification command"
  repository_gate: "Exact repo-defined gate; mark unavailable, never invent success"
review: "Independent, read-only review of the exact candidate diff"
stop_if: ["Missing required access", "Unclear data risk", "Contradictory requirements"]
```

Only request `push`, `submit`, `merge`, `deploy`, `publish`, or other durable/external capabilities when explicitly authorized by the task and repository policy. An agent must not infer permission from general access to tools.

## Execution and review rules

1. **Establish a baseline.** Identify current commit, existing test failures, required dependencies, and the smallest relevant reproduction. For behavioral bugs, add a failing regression test before applying the fix when feasible. If reproduction is impossible, explain what evidence is missing.
2. **Assign exclusive change ownership.** Use a separate branch/worktree per implementing agent. Do not share uncommitted edits or write to another agent's state. Separate databases, credentials, ports, build artifacts, and cache namespaces when relevant. Read-only reviewers may inspect others' changes.
3. **Make the smallest coherent change.** Honor the scoped paths and non-goals. Do not introduce fallback authorization, weaken tests, fabricate benchmarks, or modify unrelated workflows to turn a failure green.
4. **Verify incrementally.** Run focused tests first, then the repository's prescribed checks, then the broader risk-specific tests. Label a check `passed`, `failed`, `skipped`, `pending`, or `unavailable`; do not collapse unknown into passed.
5. **Request independent review.** The reviewer inspects the exact candidate diff with no write authority, checks correctness and security, and records actionable findings. Resolve findings and rerun affected checks after changes. A review of an old SHA is stale.
6. **Hand off exact-head evidence.** Record the full base and head SHAs and the commands actually run. A new commit invalidates prior exact-head review/verification unless the applicable gate explicitly revalidates it.
7. **Merge only after required CI gates pass on that head.** A successful local test is not proof of CI or deployment. Never auto-merge when CI is absent, queued, stale, or unavailable.

## PR evidence packet

Include these fields in the PR body or linked handoff artifact:

```text
Task / acceptance criteria:
Base SHA / head SHA:
Scope: changed files and intentional exclusions
Before: confirmed failure or baseline (or explicitly not reproduced)
After: behavior and focused test evidence
Commands run: command -> result (pass/fail/skipped/unavailable) -> environment
Security/compatibility: boundaries assessed, regressions covered
Independent review: reviewer, exact SHA, verdict/findings (or pending)
CI: workflow/run URL, exact SHA, status (or pending/not yet run)
Benchmark: machine/workload/base/candidate/variance (only when relevant)
Unverified: remaining checks, assumptions, limitations
Rollout/rollback: when deployment or migration is in scope
```

Reviews and checks must be attributed to the SHA they actually evaluated. Do not claim real-world performance or production success from a synthetic fixture. Prefer proof of an observable outcome over PR size, line counts, or contribution totals.

## Stop conditions

If the repo is dirty unexpectedly, required data or privileges are unavailable, another agent owns the path, a task would cross a sensitive boundary, or the root cause cannot be established, stop risky changes. Produce a reproducible handoff containing the blocking command, observed output (redacted), candidate next step, and what remains unknown. Do not silently skip the gate.
## Nulang execution profile

Follow [repository guidance](../AGENTS.md), [contributing rules](../CONTRIBUTING.md), [governance](../GOVERNANCE.md), and [the semantic stabilization contract](SEMANTIC_STABILIZATION_CONTRACT.md). Use isolated worktrees via `scripts/worktree-new.sh` where supported, and never run competing compiler/runtime implementations in one checkout.

```bash
# Relevant focused regression before and after changing behavior:
cargo test --locked test_name
# Local agent loop (requires just and cargo-nextest):
just fast-check
# Default-feature pre-merge gate:
just check
# Broader CI reproduction when scope warrants it:
bash scripts/ci-local.sh --full
```

Use commands actually available on the host; `just fast-check` requires `cargo-nextest`. If a prerequisite is unavailable, record it as unavailable and use an explicit partial alternative (such as `cargo test --locked`) without treating it as equivalent. GitHub Actions remains the authoritative matrix for feature combinations, docs, conformance, security and release checks.

Compiler changes need a semantics regression test and applicable bytecode/JIT/WASM parity checks. Runtime changes need actor isolation, supervision, durability/recovery, backpressure, and network tests as applicable. Performance changes require paired baseline/candidate data with equal compiler flags, feature flags, machine, workload, warm-up and repeated runs; report variability and correctness together. Mark claimed speedups unverified when measurements are absent.

Keep public-language changes within the Frozen/Stable/Experimental policy and core admission rule. Require a human maintainer to approve language-surface changes, unsafe memory/FFI logic, format changes, or durability guarantees; automated reviews supplement rather than replace maintainer approval.

## Machine-readable local evidence
Generate a read-only Git scope summary and execute exactly the checks you name (commands run **without a shell**):

```bash
python3 scripts/agent_evidence.py \
  --base <FULL_BASE_SHA> \
  --check 'focused=cargo test --locked test_name' \
  --check 'fast=just fast-check' \
  --output .git/agent-evidence.json

python3 -m unittest discover -s scripts/tests -p 'test_agent_evidence.py'
```

The base must be an existing full 40-character commit SHA. Only explicit `--check` commands run, and the collector limits them to 20 named checks with a per-check timeout. The emitted JSON records base/head SHAs, changed paths, clean/dirty state, command fingerprints, exit codes, and duration. It intentionally discards command output and does **not** claim CI, independent review, or benchmarks have passed. Use an ignored/local output path such as `.git/agent-evidence.json` and inspect the artifact before attaching it to a PR. Follow `just check` and the GitHub Actions matrix as the authoritative pre-merge gates.

