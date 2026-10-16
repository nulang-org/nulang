# Maintenance-change benchmark (experimental)

This is a **measurement protocol**, not evidence that Nulang is currently easier
to maintain than Go or Rust. Existing cross-runtime Savina benchmarks measure
runtime behavior; this benchmark measures change amplification across *one
independently verified application task*. The tool never runs commands from
submitted manifests.

## Protocol

1. Implement the same **production-like task** in independent Nulang, Rust, and
   Go applications (for example: crash-safe order fulfillment with an external
   payment-provider fake). Keep required behavior, external-effect semantics,
   failures, concurrency, and validation cases equivalent. Use idiomatic
   dependencies for each language; do not artificially penalize a baseline.
2. Complete a working baseline in each repository and record its **full 40-digit
   Git commit SHA**. Finish the same subsequent maintenance change (such as
   adding an idempotent retry boundary) and record another full SHA.
3. Run application-specific verification against each candidate revision,
   including forced crashes and replay consistency where relevant. Link to a
   retained test log or CI run; record `failed` or `not_run` honestly. Capture
   engineer time separately if measured under a consistent protocol.
4. Fill out a JSON manifest for that *one task* (example below), then run:

   ```sh
   python3 scripts/maintenance_bench.py benchmarks/maintenance/my-task.json \
     --output /tmp/nulang-maintenance-report.json
   python3 -m unittest discover -s scripts/tests -p 'test_maintenance_bench.py'
   ```

   `repo` is relative to the manifest's directory (or an absolute path).
   Keep the manifest and logs with the study or archive them with the CI
   artifact. Inputs are read-only; `--output` is the only file written.
5. Manually verify **behavioral parity**, including deployment and operations
   differences, before drawing comparisons. `ready_for_human_parity_review`
   means only that all three entries exist and report passed verification;
   it is **not** a claim that the implementations are equivalent.

## Manifest template

`base_sha` and `head_sha` must be real, locally available *full* commit SHAs.
The example contains deliberately invalid placeholders and is not runnable as-is.

```json
{
  "schema_version": 1,
  "scenario": "durable-order-processing",
  "task_id": "introduce-idempotent-payment-retry",
  "implementations": [
    {
      "language": "nulang",
      "repo": "../../../nulang-order-app",
      "base_sha": "REPLACE_WITH_40_CHARACTER_BASE_COMMIT_SHA",
      "head_sha": "REPLACE_WITH_40_CHARACTER_HEAD_COMMIT_SHA",
      "verification": {
        "status": "not_run",
        "command": "nula test",
        "evidence": ""
      },
      "change_minutes": null
    },
    {
      "language": "rust",
      "repo": "../../../rust-order-app",
      "base_sha": "REPLACE_WITH_40_CHARACTER_BASE_COMMIT_SHA",
      "head_sha": "REPLACE_WITH_40_CHARACTER_HEAD_COMMIT_SHA",
      "verification": {
        "status": "not_run",
        "command": "cargo test --locked",
        "evidence": ""
      },
      "change_minutes": null
    },
    {
      "language": "go",
      "repo": "../../../go-order-app",
      "base_sha": "REPLACE_WITH_40_CHARACTER_BASE_COMMIT_SHA",
      "head_sha": "REPLACE_WITH_40_CHARACTER_HEAD_COMMIT_SHA",
      "verification": {
        "status": "not_run",
        "command": "go test ./...",
        "evidence": ""
      },
      "change_minutes": null
    }
  ]
}
```

## What the JSON means

The report records `files_changed`, `lines_added`, `lines_deleted`, binary
files (excluded from line counts), and dependency-manifest paths touched.
Renamed files count once when Git detects the rename. Inputs are compared using
`git diff` between specified commits; **uncommitted local work is ignored**.
`change_minutes` is self-reported and is **not** inferred from commit dates.

These metrics are indicators, not a substitute for time-to-repair, upgrade
work, dependency/security maintenance, outage handling, complexity, or actual
user-visible defects. Smaller diffs can be worse code. A language/runtime
comparison should publish the complete application source, compiler/toolchain
versions, verification evidence, parity assessment, environment, maintenance
changes, and any excluded work. Never claim an improvement ratio from this
report alone. Do not publish a leaderboard until independent reproducible
results exist.
