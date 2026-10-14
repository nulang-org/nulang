# Persisted strict typed actor admission on recovery (Experimental)

This change builds on receipt-backed typed admission (PR #1721), libSQL inbox
receipts (PR #1718), and the strict mailbox admission guard (PR #1699).

## Contract

A trusted host calls `Runtime::enable_strict_typed_actor_admission(actor_id)`.
The runtime must first commit a monotonic, versioned strict-admission marker to
the chosen `PersistenceStore`. Only after that commit succeeds may it enable
the actor's in-memory strict mailbox gate.

- `MemoryStore` implements the reference policy with a per-actor set.
- `LibsqlStore` persists one row in
  `durable_actor_admission_policy(actor_id, policy_version, strict_required)`,
  where `policy_version=1` and `strict_required=1`. The policy survives
  snapshot compaction and database close/reopen.
- Other backends return `Unsupported` when a caller tries to opt in; they
  retain legacy recovery behavior until an equivalent durable policy is
  implemented for those backends.

Strict opt-in is **monotonic**: no API allows silent downgrade back to legacy
user-handler execution. A failed policy write does not enable the live gate.

## `Runtime::recover_actor` behavior

Before publishing or enqueuing a recovered actor:

1. Load strict policy. Storage corruption/read errors fail closed. A backend
   explicitly reporting `Unsupported` uses historical non-strict recovery;
   that backend cannot opt in via the strict policy API.
2. Require registered compiler-owned metadata for a persistent, non-agent,
   non-workflow actor whenever strict policy was persisted.
3. Reject an atomic tail with an epoch different from the safe snapshot, or
   a sequence ahead of that snapshot. Such history can represent a
   committed-but-unfinished command; legacy journal replay is unsafe.
4. Reject any journal entries after the safe snapshot rather than executing
   the normal non-workflow bytecode replay loop.
5. Install the strict mailbox receive gate before actor publication. Ordinary
   user messages remain queued for explicit receipt-backed admission, while
   system-priority supervisor messages retain their existing path.

This is deliberately **fail closed rather than automatically replaying**
an admitted but incomplete command. Recovering it requires a separate
compiler-verified staged handler/receipt reconstruction protocol. Existing
ordinary actor recovery behavior is unchanged unless strict policy has been
persistently enabled.

## Boundaries still needing proof

- **No automatic replay or recovery of incomplete strict turns yet.** A
  committed admission with no final snapshot is refused, not silently
  rerun. Host operators must investigate and recover using a safe protocol.
- Actor migration, placement reassignment, alternative recovery constructors,
  and external provider dispatch must enforce the same persisted policy.
  This patch covers `Runtime::recover_actor`; it is not a complete
  distributed-placement guarantee.
- Durable strict policy is not an authentication boundary for hosts that
  can mutate the database or directly invoke lower-level actor internals.
  Reconciliation across different persistence backends is not supported.
- The current runtime still needs automatic compiler-checked staged
  execution and process-kill tests for admission/result/effect/receipt windows
  before a production durability claim.
- Existing actor snapshots and NBC/NUL0 formats are unchanged. The new
  database table is additive and does not rewrite legacy journal history.

## Required verification

```sh
cargo fmt --all -- --check
cargo test --locked --test strict_actor_recovery_policy
cargo test --locked --test typed_actor_inbox_receipt_integration
cargo test --locked --test strict_typed_durable_admission
cargo test --locked --features sqlite --test durable_inbox_libsql
cargo test --locked --test runtime_snapshot_schema_identity
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Keep draft until exact-head CI and a complete code review pass. No merge or
deployment is authorized by this experimental contract.
