# Strict typed actor SIGKILL recovery matrix

This acceptance slice adds a **real process boundary** to the durable inbox
and strict-recovery draft stack. It is not a mocked store failure.

`tests/strict_actor_process_kill_libsql.rs` is compiled on Unix with the
`sqlite` feature. Each parent test launches this integration-test executable
a second time with `--exact durable_process_kill_worker`, selects a precisely
defined durability boundary, and waits for a child-created ready marker.
The parent then calls `Child::kill()` (SIGKILL on Unix), waits for exit status
signal 9, reopens the **same on-disk libSQL database**, and validates the
durable records and recovered actor state.

The child never drops the store gracefully before being killed. A bounded
20-second ready deadline and a child-process cleanup guard prevent a failed
test from leaving a live writer holding the database.

| Crash window | Durable records before kill | Expected after reopen |
| --- | --- | --- |
| Before admission | Strict policy + safe snapshot only | No inbox receipt, journal entry or atomic tail; recovered actor remains strict |
| After admission | Strict policy + inbox receipt + command journal/tail, **no completed state** | Retry returns original receipt; strict actor recovers quarantined with the committed command fenced and **no handler replay** |
| After completion | Strict policy + original receipt + journal plus completed state snapshot/tail | Retry remains duplicate; completed state and strict mailbox policy recover without rerunning the command |

The test uses the actual compiled `Counter` metadata in the recovery
runtime. A uniquely identified incomplete admission may recover as a
**quarantined actor** with an in-flight completion fence; it is never
automatically executed. Ambiguous history still refuses recovery. This is a test of *at-most-once replay rejection
at these chosen boundaries*, not of lossless intake or exactly-once effects.

## Not yet established

- The signal fires **after** a committed API call returns (or before the
  admission call starts). This is not a mid-transaction forced-kill/fault
  injection test; SQLite atomic rollback is not separately proved here.
- Provider dispatch, outbox delivery and external acknowledgments are not
  exercised.
- The integration does not yet implement staged, automatically resumable
  compiled handlers or authenticated sender retries.
- Alternate actor migration/placement paths and non-libSQL durable backends
  are outside this matrix.
- There is no real host failover/rolling replacement test, nor is there
  cross-process concurrency at the moment of kill.

## Exact-head verification

```sh
cargo fmt --all -- --check
cargo test --locked --features sqlite --test strict_actor_process_kill_libsql -- --nocapture
cargo test --locked --test strict_actor_recovery_policy
cargo test --locked --features sqlite --test durable_inbox_libsql
cargo test --locked --features sqlite --test durable_inbox_identity
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

All contributors to this stacked draft must pass exact-head CI before
promotion. Do not present the new tests as passed until they have run on a
host with Rust, local SQLite/libSQL support and Unix process-kill semantics.
