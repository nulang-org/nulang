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
3. Reject a mismatched activation epoch or multiple atomic commands ahead of
   the safe snapshot. If the atomic tail is **exactly one sequence** ahead,
   require exactly one journaled command at that sequence and reject orphaned
   or ambiguous journal history or post-snapshot domain events.
4. For that single admitted-but-unfinished command, restore the actor with an
   **admission fence at its committed command sequence**, without invoking its
   bytecode or native handler. No user work can pass the fence until completion.
5. Install the strict mailbox receive gate and (if necessary) the admitted
   command fence **before actor publication**. System-priority supervisor
   messages retain their existing path.

Recovery of a proven pending command now returns a **quarantined, fenced
actor**, not a runnable handler. The trusted host may durably commit an
already-authorized staged result and call `finish_admitted_typed_actor_command`
to validate the completion snapshot, hydrate memory and release the fence.
This does not produce the staged result or execute compiled bytecode. Existing
ordinary actor recovery behavior is unchanged unless strict policy has been
persistently enabled.

## Boundaries still needing proof

- **No automatic execution of incomplete strict turns.** Exactly one
  proven unfinished admission can be restored in a quarantined state, but
  ambiguous history is refused. Host operators must supply an independently
  replay-safe, already-authorized staged completion; the runtime never
  invents state or re-runs unrestricted bytecode.
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
