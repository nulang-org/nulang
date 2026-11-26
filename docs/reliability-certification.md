# Reliability certification contract

Nulang correctness needs both language semantics and fault recovery evidence. Compilation success alone is not runtime certification.

## Required evidence

1. **Exact revision:** record the Git SHA and Cargo feature profile for each result.
2. **Small deterministic matrix:** compile and test default, `--no-default-features`, and SQLite-backed durability features.
3. **Crash/replay:** validate durable receipts, actor activation epochs, WAL recovery, timers, snapshots, and provider-effect replay.
4. **Fault scheduling:** replay deterministic failing seeds and preserve minimized reproductions as regression tests.
5. **Promotion:** require exact-head compiler/runtime CI plus concrete recovery evidence, not just documentation or release binary creation.

## Focused executable suite

Run `bash scripts/ci-local.sh --reliability` for the crash/replay matrix and a bounded deterministic seed sweep.

- `bash scripts/test-durability-guarantees.sh` uses minimal features for compiler, receipt, WAL, and snapshot tests.
- The SQLite-gated `workflow_effect_tail_recovery` and `workflow_timer_process_restart_replay` tests run under `--no-default-features --features sqlite`. Running them under `--no-default-features` alone reports zero tests and **does not** count.
- `bash scripts/tests/durability-guarantees-contract.test.sh` verifies the dispatch contract and fail-fast behavior with mocked Cargo. It is *not* a replacement for the actual Rust tests.
- `.github/workflows/dst-nightly.yml` exercises a larger seed sweep; it is independent evidence, not an exact-head PR gate.
- `tests/workflow_timer_process_restart_replay.rs` now reopens the durable actor twice and asserts exactly one logical timer record and one in-memory timer after both recoveries.

## Fail-closed policy

Pending, skipped, stale-head, zero-test, or unexecuted target-profile results are **not certified**. Preserve both deterministic seeds and input artifacts when reporting reproducibility.

Future work: process-kill recovery and cross-node partition tests using durable stores under realistic I/O failure injection.
