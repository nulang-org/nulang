# Strict typed actor staged-result SIGKILL matrix (Experimental)

This draft extends the [strict staged-result commit boundary](STRICT_QUARANTINE_STAGED_RESULT.md)
and existing inbox SIGKILL coverage. The objective is to distinguish a crash
**inside a completion transaction** from a crash **after completion COMMIT but
before the host receives acknowledgment**.

## Failure boundaries

| Window | Persisted state before kill | Required behavior after reopening the same WAL-backed file |
| --- | --- | --- |
| During staged completion | Original safe snapshot, strict policy, atomic inbox receipt and admitted command sequence 1; completion snapshot, Prepared effect intent, outbox and transition row written *inside an uncommitted transaction* | All completion records roll back; admission and receipt remain; strict actor recovers **quarantined** with sequence-1 fence; trusted host can retry completion exactly once |
| After completion COMMIT, before acknowledgment | Completed snapshot and transition tail at sequence 2, Prepared effect intent and outbox, plus original admission receipt | Committed state and intents survive; exact completion retry is idempotent; there are no duplicate outbox or effect rows; duplicate inbox receipt is an acknowledgment, not new handler work; recovered actor has strict policy and completed state |

The parent tests run a real separate process, wait for the selected commit
boundary, send SIGKILL, reap the process and reopen the same local SQLite/libSQL
database. Timeouts and child cleanup prevent stranded writers.

For the in-transaction case, the explicit
`durability-fault-injection` Cargo feature enables an actor-scoped hook
**after** the completion snapshot, Prepared effect records, outbox and
`durable_transitions` INSERT statements, but **before** the committed
`durable_tails` update and SQL COMMIT. A real process kill at that point
tests rollback of all staged completion records. The original command's
receipt is from an earlier committed transaction and must survive.

The post-COMMIT case does not use a transaction hook. It writes a ready marker
only after `commit_transition` returns success; the parent SIGKILLs the child
before any host acknowledgment can be delivered.

## Execution

The existing isolated `durable-inbox-inflight-crash` CI job now runs both
the inbox and staged-result crash suites:

```sh
cargo test --locked --no-default-features \
  --features durability-fault-injection \
  --test libsql_inflight_inbox_sigkill -- --nocapture

cargo test --locked --no-default-features \
  --features durability-fault-injection \
  --test strict_staged_completion_sigkill -- --nocapture
```

In addition, exact-head promotion requires formatting, default feature tests,
strict actor admission/recovery tests, and Clippy checks.

## Guarantees not provided

These tests use the **real production storage transaction** with host-supplied
completion records. They do not execute compiled handlers, intercept tools,
contact an external provider, or dispatch the outbox. An idempotent database
commit is not the same as exactly-once provider execution or exactly-once
cross-node delivery.

The currently exposed `commit_admitted_typed_actor_result` method still
trusts the host to construct deterministic, authorized state updates and
Prepared effect intents. A compiler-controlled pure-handler staging sandbox,
receipt-fenced external dispatcher, cross-process concurrency/placement
fencing, and additional fault windows remain separate P0 milestones.

The fault-injection feature is explicitly opt-in and must never be enabled in
production builds. The tests are authored but **not claimed to be passing**
until their exact-head CI run executes successfully.
