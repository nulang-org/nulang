# Experimental task output commit contract

This is a small, Hadoop MapReduce-inspired **semantic contract**, not an
end-to-end distributed task execution service. The implementation is in
`src/task_output_commit.rs`, and the executable specification is
`tests/task_output_commit.rs`.

## Core invariant

One logical task has a monotonically increasing fencing epoch and at most one
authoritatively committed result. The controller explicitly assigns an
`AttemptToken { task_key, attempt_id, epoch }`; the worker stages the digest
and immutable artifact key; and only the current attempt may commit that
staging record.

- A stage is not a visible result.
- Reassigning the task increments the epoch and discards the old staged
  reference; a stale attempt cannot stage or commit.
- Within an epoch, retrying the same stage is idempotent; changing the
  artifact key or digest fails closed.
- Committing publishes the exact staged metadata; an exact repeated commit
  returns `AlreadyWon`; a different result cannot replace the winner.
- A versioned checkpoint preserves the epoch, staged metadata, and committed
  winner across a clean restore. Invalid and future versions are rejected.
- Each token includes the task key to prevent cross-task credential confusion.

**Do not treat an AttemptToken as proof of authorization.** A trusted host must
authenticate the worker and bind the attempt to a durable allocation lease.
The state-machine API does not contain a physical clock, lease expiry,
cryptographic identity, or a signed provider capability.

## Atomic persistence adapter (experimental)

`DurableTaskOutputStore` now persists the state into a dedicated actor slot
through the existing `PersistenceStore::commit_transition` API. It uses an
epoch-fenced `DurableTransition` and writes the complete versioned task state
as a single `EventEntry` in the atomic domain-event journal. It does **not**
change the database schema or create a parallel distributed lock.

A transition's `expected_previous_sequence` is its compare-and-swap
precondition. A losing writer receives a storage conflict and must re-read
durable authority; the adapter never silently rebases its stale proposal.
`MemoryStore` supports deterministic contract tests. The `LibsqlStore` path
is covered by a file-backed reopen test and a two-independent-writer race.

Recovery requires complete, ordered task checkpoint events and an authoritative
durable tail whose sequence and activation epoch match the recovered state.
Unknown/legacy backends and foreign occupant history fail closed. This first
adapter deliberately uses one dedicated actor slot **per task**; an independent
allocator must guarantee slot uniqueness and tenant-scoped access.

The current stream reader scans and validates the entire checkpoint history.
That is acceptable for a bounded experimental contract, but high-throughput
production scheduling requires a targeted latest-state read and bounded
compaction/snapshotting without relaxing recovery checks.

## Required production integration (NOT implemented here)

Nulang Cloud / the owning durable-task adapter must implement:

1. Authoritatively establish that the old owner has been fenced or
   definitively removed. Gossip suspicion and heartbeat expiry alone are not
   acceptable proof. Allocate the next epoch with a store-side CAS.
2. Use the `DurableTaskOutputStore` epoch-and-sequence guarded transition
   for each task's assigned slot. Resolve CAS conflicts by reloading and
   re-authorizing; do not issue blind retries or bypass `commit_transition`.
   A JSON-file overwrite, eventually consistent cache, or unsynchronized
   read-modify-write is not sufficient.
3. Verify immutable staged artifact bytes against the digest and bind the
   object key to the authenticated tenant, task and attempt. Never publish or
   reference worker-controlled mutable locations as authoritative outputs.
4. Commit the winner under CAS before notifying consumers or advancing the
   dependent workflow. Failed or fenced stages should be garbage-collected only
   after the relevant retention/recovery horizon.
5. Integrate with Nulang's `DurableTransition` actor/workflow journals and
   `DurableEffectCoordinator` where results affect durable state. Use the
   existing stable external operation ID and provider-side deduplication; local
   output single-winner semantics do **not** prove exactly-once remote effects.
6. Reconcile this generic contract with the separate, open NLAP attempt/lease
   work in [#1137](https://github.com/nulang-org/nulang/pull/1137) before
   promoting either API; do not fork production fencing authorities.

An atomic store adapter now exists, but this remains **not production-safe
distributed publication** until authenticated lease enforcement, cross-process
kill/crash verification and deployment-specific failure-domain acceptance are
proven. Two-controller SQLite contention only tests one local database file,
not remote failover or power-loss consistency.

## Validation

```bash
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test --locked --no-default-features --test task_output_commit
cargo +1.95.0 test --locked --no-default-features --test task_output_durable_store
cargo +1.95.0 test --locked --no-default-features --features sqlite --test task_output_durable_store
bash scripts/test-durability-guarantees.sh
cargo +1.95.0 test --locked
```

The lightweight tests cover staged-output invariants, token fencing,
idempotent retries and invalid-checkpoint rejection. The SQLite feature gate
adds actual file reopen and two-writer local database contention. They do
**not** prove kill-during-fsync durability, remote database semantics,
authenticated worker admission, external-effect idempotence, or two live
controllers separated by a network partition. Those remain mandatory
acceptance gates before the draft can become merge-ready.
