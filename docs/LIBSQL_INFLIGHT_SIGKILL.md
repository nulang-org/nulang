# Mid-transaction durable inbox SIGKILL acceptance

This experimental test complements the boundary-oriented process-kill matrix
in draft PR #1723 and the libSQL atomic transition rollback tests from #1718.
It tests the **actual `LibsqlStore::commit_inbox_transition` implementation**
while its SQLite `IMMEDIATE` transaction is open.

## Fault window

The optional `durability-fault-injection` Cargo feature adds one inert-by-default
test hook immediately after the inbox command's `journal` INSERT. At that
point, a transition containing a snapshot and a command has written both
inside the transaction, but has not inserted an inbox receipt, updated the
atomic tail, or committed.

When a subprocess provides the exact actor-specific opt-in environment
variables, the hook writes a ready marker and parks forever. The parent
test observes the marker and sends **real SIGKILL**, bypassing Rust destructors
and any graceful SQLite rollback. It then reopens the same WAL-backed file.

The parent checks:

1. The *previously committed* safe snapshot and strict admission marker survive.
2. The new snapshot and journal entry are absent, and no new transition,
   tail, receipt or outbox record is visible.
3. Retrying the same logical sender message is accepted as **new**, producing
   exactly one journal entry, committed snapshot, receipt and outbox record.

This proves SQLite rollback and the inbox store's consistency at this
precisely instrumented in-flight point **when the test passes**. The
existing PR #1718 failure-injection test separately proves rollback if a
receipt INSERT fails after later records have already been staged.

## Safety and build isolation

The hook is not present in the normal `default`, `full`, or `release-cli`
feature sets. It requires the explicitly dangerous
`durability-fault-injection` feature, together with matching stage, actor ID,
and ready-file environment variables. The test is Unix-only.

Do not include this feature in production builds, SDKs, or release artifacts.
It is intentionally capable of indefinitely suspending an in-flight
transaction when explicitly armed. The test's parent process has a bounded
readiness timeout and child cleanup guard to avoid orphaned writers.

## Exact-head verification

```sh
cargo fmt --all -- --check
cargo test --locked --no-default-features \
  --features durability-fault-injection \
  --test libsql_inflight_inbox_sigkill -- --nocapture
cargo test --locked --features sqlite --test durable_inbox_libsql
cargo test --locked --features sqlite --test strict_actor_process_kill_libsql
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D clippy::correctness
```

The dedicated `durable-inbox-inflight-crash` CI job exercises the test with
a sqlite-only Rust feature profile.

## Still outside this proof

No fault is injected during transition digest serialization, inbox-receipt
INSERT, atomic-tail update, or provider dispatch. An injected SIGKILL proves
neither automatic replay-safe bytecode resumption nor exactly-once effects
against external providers. Those are separate P0 workstreams.

The experiment is draft-only pending actual exact-head CI and review.
