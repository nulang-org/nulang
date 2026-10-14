# Trusted staged completion of quarantined typed actors (experimental)

This integration builds on the strict-admission and quarantine stack through
PR #1727, and **adapts** the prior trusted-host atomic-result adapter from
draft PR #1697. The previous draft is not merged or modified; its compatible
validation logic is carried into this stacked branch with stricter admission
and historical effect-ID checks.

## Intended transition sequence

1. A trusted host supplies a stable inbox identity and a compiled typed
   command. The runtime atomically admits the command with its inbox receipt.
   Its mailbox is fenced at the admitted sequence.
2. If the process terminates before a result is committed, strict recovery
   restores a **quarantined**, fenced actor from its last safe snapshot.
   It does not run the original handler.
3. A trusted, replay-safe host computes a pure, typed state delta and
   optional `Prepared` effect intents / outbox records independently.
   **This adapter does not execute compiler bytecode, evaluate a handler,
   contact providers, or deliver the outbox.**
4. `Runtime::commit_admitted_typed_actor_result(actor_id,
   admitted_sequence, state_updates, durable_effects, outbox)`
   validates the persisted strict policy, the specific actor admission
   fence, compiler-owned schema, primitive durable fields, effect authority
   grants, effect state and logical IDs.
5. It builds a `DurableTransition` at `admitted_sequence + 1`, with a
   complete snapshot and all intents, and commits everything using one
   atomic `PersistenceStore::commit_transition`.
6. Only after the commit succeeds does the existing
   `finish_admitted_typed_actor_command` revalidate the committed tail,
   schema, activation epoch, authority and state, hydrate actor memory,
   and release the admitted-command fence. A storage failure leaves state
   and the fence untouched.

## Rejection boundaries

- Requires the **monotonic persisted strict admission policy**. A transient
  mailbox flag by itself never authorizes crash-safe completion.
- Only compiler-owned, persistent, non-workflow/non-agent actor state is
  supported; CRDT and event-sourced state are not flattened.
- Reject undeclared fields and types incompatible with primitive compiler
  defaults. Structural types need real compiler-emitted descriptors.
- Reject external effect records outside the `Prepared` state, duplicate
  effect IDs within a result, and effect IDs already in durable history.
- External effect operations must parse as canonical authority grants and
  be permitted by the live actor manifest.
- Outbox entries are **data**, not dispatched providers. Delivery must
  subsequently use its own fenced, idempotent dispatch boundary.

## Still not production-ready

The host is trusted to provide a replay-safe result and authenticated
inbox identities. No sandboxed compiler handler staging or deterministic
bytecode recording exists in this change. A host that executes arbitrary
side-effecting user bytecode before the commit can still duplicate external
effects; callers must not do that.

The method is not a general durable orchestration or exactly-once provider
API. Alternative persistence backends, multi-process fencing, cross-node
placement, explicit outbox dispatch receipts and crash-window E2E proofs
still require work. All stacked parents are drafts.

## Exact-head test gate

```sh
cargo fmt --all -- --check
cargo test --locked --test strict_typed_staged_result
cargo test --locked --test strict_actor_recovery_policy
cargo test --locked --features sqlite --test strict_actor_process_kill_libsql
cargo test --locked --features sqlite --test durable_inbox_libsql
cargo test --locked --no-default-features --features durability-fault-injection \
  --test libsql_inflight_inbox_sigkill -- --nocapture
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D clippy::correctness
```

These commands are promotion requirements, not statements that tests
have already passed. Do not merge until exact-head CI is green.
