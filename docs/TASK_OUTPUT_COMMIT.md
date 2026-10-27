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

## Required production adapter (NOT implemented here)

Nulang Cloud / the owning durable-task adapter must implement:

1. Authoritatively establish that the old owner has been fenced or
   definitively removed. Gossip suspicion and heartbeat expiry alone are not
   acceptable proof. Allocate the next epoch with a store-side CAS.
2. Persist the entire updated `TaskOutputState::checkpoint()` in **one atomic
   durable compare-and-swap** guarded by the prior record version. A process
   must reload and retry from the winning record after a CAS conflict.
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

Until an atomic backend, worker dispatch adapter, and cross-process crash and
two-writer race tests exist, this is **not production-safe distributed
publication**. A logical in-memory state machine is not a distributed lock.

## Validation

```bash
cargo +1.95.0 fmt --all -- --check
cargo +1.95.0 test --locked --no-default-features --test task_output_commit
bash scripts/test-durability-guarantees.sh
cargo +1.95.0 test --locked
```

The lightweight tests prove local staged-output invariants, token fencing,
idempotent retries, checkpoint restoration and invalid-checkpoint rejection.
They do **not** simulate concurrent durable CAS, a killed process during fsync,
external side effects or two live controllers in a network partition. Those
remain mandatory acceptance gates before the draft can become merge-ready.
