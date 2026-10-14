# Receipt-backed typed actor admission (Experimental)

This integration branch combines the existing strict typed actor admission
work from PR #1699 with the durable inbox identity, memory, and libSQL receipt
stores from PRs #1704 and #1718. These independent draft stacks remain
unchanged and **none of the components is promoted to Stable**.

## API

Trusted hosts can call:

```rust
runtime.enable_strict_typed_actor_admission(actor_id)?;

match runtime.admit_next_typed_actor_command_with_inbox(
    actor_id, authenticated_inbox_key, compiled_transition,
)? {
    Some(TypedInboxAdmission::New { message, commit }) => {
        // The message is durably admitted and fenced, but bytecode has NOT
        // executed. Stage effects and state in a trusted, replay-safe handler.
        // Do not dispatch provider effects until durable intents are committed.
        let _ = (message, commit);
    }
    Some(TypedInboxAdmission::Duplicate { commit }) => {
        // Old acknowledgment: NEVER run the handler a second time.
        // The mailbox remains fenced until trusted state hydration/recovery.
        let _ = commit;
    }
    None => {} // no ordinary user message is eligible for admission
}
```

Only `New` yields a message to run. The backend's
`commit_inbox_transition_detailed` reports `newly_accepted` **from inside
the atomic storage transaction**, not from a racy independent receipt read.

The adapter validates compiler-owned actor/behavior ownership, arity,
lossless canonical payload encoding, expected actor and activation epoch, and
the exact durable command envelope. A rejected store commit retains the
original mailbox candidate. The strict gate blocks legacy bytecode dispatch,
selective receives and synchronous asks while allowing system supervisor
messages to continue.

For a replayed receipt:

- Missing or incompatible epoch-fenced completion snapshot => `WouldBlock`
  and the mailbox candidate is retained. A journal receipt alone is not proof
  that the handler committed state.
- A compatible completion snapshot => consume the duplicate envelope without
  executing bytecode, but **fence** the actor at its admitted sequence.
  The trusted host must explicitly call
  `finish_admitted_typed_actor_command` to verify state, schema and
  authority and hydrate live state before the next user command can run.
- If the durable tail has advanced too far for the restricted completion
  adapter, leave the actor fenced and restore the activation from validated
  storage. Never unconditionally clear the fence.

## Remaining limits

**This is not an end-to-end durable execution guarantee.** The strict flag is
process-local; recovery must explicitly enable it before accepting traffic.
The current `Runtime::recover_actor` path does not persist or automatically
restore that policy. Hosts must ensure this ordering externally until a
versioned persisted activation policy is integrated.

The host also still controls compiled handler execution; there is no
automatic bytecode staging, effect interception, provider-dispatch
authorization, or replay-safe outbox publication. Incoming uncommitted
process-local mailbox messages remain vulnerable to loss without retries.
For a pending committed command without a completion snapshot, the host must
resume through the explicit recovery protocol and must not re-run
unrestricted bytecode or duplicate external effects.

This API trusts the host to supply authenticated, stable namespace/sender/
message identities. Receiver-derived content hashing cannot substitute for
the sender's logical message key.

## Exact-head verification

```sh
cargo fmt --all -- --check
cargo test --locked --test typed_actor_inbox_receipt_integration
cargo test --locked --features sqlite --test durable_inbox_identity
cargo test --locked --features sqlite --test durable_inbox_libsql
cargo test --locked --test strict_typed_durable_admission
cargo test --locked --test atomic_persistent_mailbox_admission
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Next P0: persist and automatically enforce strict activation policy before
mailbox traffic on restart; integrate isolated compiled handler staging; then
run actual process-kill tests across admission/commit/dispatch/receipt
windows. No exactly-once claims against external systems.
