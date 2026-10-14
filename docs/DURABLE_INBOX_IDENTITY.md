# Durable inbox identity and deduplication (experimental)

The storage-neutral `DurableInboxCommand` contract establishes one logical
incoming message identity **independently** of receiver activation epoch or
receiver journal sequence.

## Why this is needed

An atomic journal command is not equivalent to a crash-safe inbox. If a host
receives the same network command again after a commit, or retries after losing
the acknowledgment, it needs an identity preserved by the sender. If the
receiver blindly allocates a new journal sequence, a logical command can run
twice.

`DurableInboxCommand::new(namespace, sender, message_key, actor_id, behavior_id, payload)`
derives two different values:

- `DurableInboxId` — BLAKE3 identity over a versioned, length-delimited
  namespace, stable sender, stable logical message key, and receiver actor ID.
- `request_digest` — separate versioned, tagged encoding of the receiver,
  behavior and **exact persisted payload**, including float bit patterns.
  Non-finite floats are rejected.

Sender, namespace and actor identity must already have a stable trusted
meaning. **Payload hashing alone is not message identity**: two intentionally
distinct commands may have exactly the same arguments. Reusing a logical key
with different arguments is a conflict, not a new command.

## Backend API

Two new optional methods on `PersistenceStore`:

- `commit_inbox_transition(request, transition)`: atomically publish the
  transition and a bound inbox receipt, or fail closed.
- `load_durable_inbox_receipt(id)`: inspect the accepted logical message and
  its original `DurableCommit`.

`MemoryStore` provides a single-owner, ephemeral reference implementation.
Repeated equal requests return their original receipt even after newer turns
commit; a conflicting request fails without mutating the actor's journal.
A normal `commit_transition` cannot retroactively mint an inbox receipt.

**Disk-backed implementations including libSQL currently return
`io::ErrorKind::Unsupported` for these methods.** They must never implement
a `load`-then-`commit_transition` sequence, since two competing hosts could
both observe an absent receipt and commit. A production backend must perform
one atomic transaction with a uniqueness constraint on
`(namespace/sender/message/receiver)` identity and verify its payload digest
under the same transaction.

## Deliberate limitations

- This is **not** recovery of an uncommitted, process-local mailbox message.
  Loss before inbox acceptance remains possible without durable transport
  delivery or sender retries.
- The existing `JournalEntry` wire and persistence formats are unchanged.
  No legacy record is assumed to contain or imply a durable inbox ID.
- Neither the sender's stable identity nor authorization is validated here:
  the authenticated transport/tenant boundary must supply and check them.
- The reference `MemoryStore` is not crash-persistent. Its atomic property
  only models the intended single-owner API semantics in memory.
- The runtime's strict admission adapter is not automatically wired to these
  APIs. Until durable backend support and runtime integration are validated,
  this cannot be advertised as exactly-once processing, lossless inbound
  delivery, or crash-safe actor execution.
- External providers still require their own idempotency keys and receipt
  reconciliation; this inbox contract controls Nulang-owned command records,
  not external effects.

## Focused tests

```sh
cargo fmt --all -- --check
cargo test --locked --test durable_inbox_identity
cargo test --locked --test durable_activation_epoch
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Keep experimental until exact-head CI passes. Next steps: implement versioned
transactional inbox records in libSQL with a real persistence/restart test;
then integrate the receipt-backed admission API into the strict typed actor
host boundary and test process kills at each admission/commit/receipt point.
