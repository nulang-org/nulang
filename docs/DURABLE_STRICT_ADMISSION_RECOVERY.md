# Persisted strict admission policy (experimental)

## Problem

Draft #1720 integrates the opt-in strict mailbox gate with isolated scalar bytecode, but the mailbox flag is process-local. An ordinary actor could recover from its completed snapshot and resume legacy dispatch before the trusted host re-applies the gate.

## Contract

`Runtime::enable_strict_typed_actor_admission(actor_id)` now writes a **versioned strict policy** in an epoch-fenced, snapshot-bearing `DurableTransition` **before** setting the live mailbox flag:

- Version `0`: legacy snapshot; no strict policy asserted.
- Version `1`: strict host-controlled admission required; the recovered mailbox is fenced before actor publication.
- Any other version: reject activation before actor publication.

The policy commit increments the durable sequence. The next command must use the committed policy transition as its expected predecessor. The API is idempotent when the exact committed policy snapshot/tail already exists, and refuses mutation while a message is reserved or admitted. Storage errors do not enable the gate.

Successful admitted-result commits preserve policy version `1` in the resulting snapshot. Both the normal `recover_actor` path and the alternate persistent spawn/restart path restore this marker. A valid atomic ordinary-actor tail from an older snapshot *without* the policy field is also fenced by default, preventing an old atomic result from restoring legacy dispatch.

An incomplete atomic command (tail beyond the last completed snapshot) still fails closed under #1700. Unknown policies, missing strict atomic tail support, invalid epochs, or mismatched strict tails must not publish a recoverable actor.

The migration receiver rejects strict-policy snapshots until it can transfer and validate both the exact tail and its admission/inbox boundary. This is intentionally safer than silently reactivating legacy dispatch on the destination.

## Storage

`ActorSnapshot.strict_admission_policy_version: u16` is `serde(default)` for backward compatibility. MemoryStore, JSON and RocksDB snapshots serialize the version as part of the struct. libSQL and PostgreSQL snapshots use an additive `strict_admission_policy_version` column with default `0` for older tables. libSQL's atomic transition path writes the version in the same transaction as the snapshot and tail. Existing PostgreSQL persistence lacks the atomic transition capability used by policy opt-in; it must reject this opt-in rather than promise durable strict admission.

## Verification

```sh
cargo fmt --all -- --check
cargo test --locked --test durable_strict_policy_recovery
cargo test --locked --test strict_pure_durable_bridge
cargo test --locked --test strict_typed_durable_admission
cargo test --locked --test pure_staged_actor_bytecode
cargo test --locked --test ordinary_atomic_turn_recovery_fence
cargo test --locked --test admitted_typed_actor_atomic_result
cargo check --all-targets
cargo check --all-targets --no-default-features
cargo clippy --all-targets -- -D warnings
```

## Not yet proven

Process-kill testing (as opposed to closing/reopening libSQL handles), stable inbox identities/deduplication, migration support for strict actors, durable external effects, outbox delivery, and promotion CI are outstanding. The implementation is a draft and must not be merged or deployed without exact-head compilation/test results and review.
