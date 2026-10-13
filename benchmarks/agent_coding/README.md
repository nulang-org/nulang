# Nulang coding-agent benchmark — scoring foundation

This directory contains the evaluation protocol. The initial implementation
is a **deterministic scorer**, not a code-generation agent runner. It does not
claim model quality or execute untrusted code. A separate trusted evaluator must
generate result rows by running Nulang compiler and behavioral tests in a
restricted disposable environment.

## Reproducibility contract

1. Pin the Nulang compiler commit and backend profile (bytecode is the
   semantic reference until equivalent backend conformance is established).
2. Pin exact model IDs, model configurations, tool affordances, prompt templates,
   maximum repair cycles, and time/token budgets.
3. For every task provide an immutable starter repository, task prompt, and
   hidden acceptance tests. Do not score compilability alone: deleting broken
   code can otherwise produce false successes.
4. Record **all** model/task/trial attempts, including timeouts, execution
   failures, and cases with missing compiler or test results.
5. Run acceptance checks outside the agent's writable workspace, on fresh
   snapshots. Never give an agent unrestricted test execution or repository
   merge authority. The existing `nula test` command executes program code;
   isolate it using an OS/VM sandbox and explicit runner permission.
6. Retain exact logs/artifact hashes to allow independent audit. The scoring
   program does not authenticate submitted outcomes.
7. Use matching task sets and trials for model comparisons; a partial run is
   marked by `coverage < 1.0`. Use paired comparisons with uncertainty
   intervals when evaluating small improvements. Do not treat single-run
   rankings as statistically reliable.

## Input data

Create a task manifest:

```json
{
  "schema_version": 1,
  "suite_id": "nulang-agent-coding-experiment-01",
  "trials": 3,
  "tasks": [
    {"id": "repair-unbound-identifier"},
    {"id": "repair-effect-row"}
  ]
}
```

The manifest above is an **illustrative specification**, not a benchmark dataset.
Tasks are not valid for publication until starter repositories and acceptance
tests are independently reviewed.

Each line in `results.jsonl` is a JSON object from the trusted evaluator:

```json
{
  "model": "provider/model-version",
  "task_id": "repair-unbound-identifier",
  "trial": 1,
  "compiler_ok": true,
  "tests_ok": true,
  "repair_cycles": 1,
  "input_tokens": 300,
  "output_tokens": 120,
  "elapsed_ms": 1000
}
```

The row is a **schema example**, not an observed benchmark result. The evaluator
must set `compiler_ok` and `tests_ok` only from actual subprocess results,
including exit codes, under the agreed execution profile. Timing values are
integer milliseconds, token counts are nonnegative integers, and trials are
1-indexed. A reported success requires both booleans to be true.

## Scoring

```sh
python3 scripts/agent_coding_bench.py \
  --tasks benchmarks/agent_coding/tasks.json \
  --results /path/to/results.jsonl

python3 -m unittest discover -s scripts -p test_agent_coding_bench.py
```

The first command requires a **real manifest** at the named path; this
repository does not currently ship a validated task set at that location.
Create a reviewed manifest, starter artifacts, and hidden verification checks
before running a model evaluation.

The report includes, per model:

- `manifest_sha256` = SHA-256 of the exact manifest bytes; only compare runs\n  with the same task and protocol identity, or document intentional differences\n- `coverage` = recorded attempts / (tasks × trials)
- `success_rate_observed` = compiler-and-test successes / recorded attempts
- `success_rate_all_tasks` = successes / expected attempts (missing = not passed)
- `tokens_per_success` = all spent tokens, including failed attempts, divided
  by successes; null if no successes
- median repair cycles and elapsed milliseconds among successful attempts

Models with incomplete coverage are not directly comparable using observed
success rates. No synthetic model scores are checked into this repository.

## Release requirements

Before publishing any benchmark comparison, provide the complete result
records, the compiler commit, model version identifiers, acceptance-test
evidence, sampler settings, task hash manifests, and failure/timeout counts.
The initial scorer is deliberately independent of PR #1656 (machine-readable
diagnostics) and PR #1657 (local MCP host). It can measure either baseline
tooling or improved tooling without assuming either PR has shipped.
