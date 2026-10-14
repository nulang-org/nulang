# Pure staged durable actor turns (experimental)

## Scope

An opt-in trusted-host API, `Runtime::execute_and_commit_admitted_pure_typed_actor_turn(actor_id, admitted_sequence)`, advances the **compiler-owned ordinary persistent actor** path from command admission to atomic result persistence for a narrow class of pure handlers.

The host first calls `admit_next_typed_actor_command` with the exact canonical command. The adapter then:

1. Checks the in-memory admitted-command fence and the exact persisted journal entry.
2. Resolves the behavior using the selected compiled actor schema and verifies parameter count.
3. Captures declared durable scalar fields **without lossy heap-pointer conversion**.
4. Preflights a bounded, straight-line instruction region **before** running any bytecode.
5. Runs on a separate `VM::new_without_jit()` with detached scalar state callbacks. Live actor state, actor/mailbox callbacks, host resources and providers are never installed in the staged VM.
6. Submits the staged state through `commit_admitted_typed_actor_result` with empty effect/outbox lists. That existing adapter atomically commits the result snapshot, validates the live fencing/authority, updates actor state and only then admits the next message.

## Accepted surface

Only scalar `Int`, `Float`, `Bool`, `Nil`, `Unit` values and compiler-owned **durable** fields are eligible. The first version supports straight-line scalar literal loads, local register moves/loads/stores, integer and float comparisons, elementary integer arithmetic and boolean operations, `StateGet`, `StateSet`, and direct returns. The instruction scan ends at the first return/halt, may not cross into another function/behavior region and enforces a 4096-instruction bound. There is no control-flow jump or function call.

Everything else — including sends/asks, actor spawn, closures, file/network IO, Python/FFI, effects, external providers, `Emit`, native calls, allocations, string/object/CRDT state, branches, and suspension — **fails closed before any user bytecode is executed**. Primitive state writes must retain the original compiled scalar type.

## Guarantees and limitations

- Rejection, interpreter errors, or incompatible staged values leave live actor state unchanged and the admitted-command fence intact. Reconciliation remains the responsibility of the trusted host.
- The method requires an **already committed command**; it is not a general-purpose actor scheduler and does not automatically enable strict admission (see draft #1699).
- Persistence commit errors remain subject to the existing exact-transition-idempotency and ambiguous-acknowledgment rules. The host must not change transition identity on retries.
- The adapter stages **no** external effects or outbox messages. It does not provide external exactly-once delivery, deduplicated incoming messages, migration-body verification or automatic resumption after restart.
- The current underlying two-transition protocol has one command-admission commit and a subsequent result commit. The conservative recovery fence in draft #1700 rejects incomplete ordinary-actor turns rather than guessing whether to run them.
- The detached VM is not a general bytecode security sandbox. Its safety claim is restricted to the statically permitted ISA subset, primitive inputs/state and no supplied external callbacks.

## Validation

At the exact PR head:

```bash
cargo fmt --all -- --check
cargo test --locked --test pure_staged_actor_bytecode
cargo test --locked --test admitted_typed_actor_atomic_result
cargo test --locked --test ordinary_atomic_turn_recovery_fence
cargo test --locked --test atomic_persistent_mailbox_admission
cargo check --all-targets
cargo check --all-targets --no-default-features
cargo clippy --all-targets -- -D warnings
```

Treat executable CI and an independent review of every allowed opcode as required before promotion. Do not infer broad deterministic replay or durable effect semantics from this restricted path.
