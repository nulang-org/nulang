# Strict typed durable command admission (Experimental)

The runtime exposes `Runtime::enable_strict_typed_actor_admission(actor_id)`
for **trusted hosts** that explicitly own persistent typed-actor command
admission and completion. Enabling it is monotonic for that actor's live
mailbox and rejects transient actors, workflows, agents, and unknown schemas.

With strict admission enabled:

- Normal and local user messages cannot leave the mailbox through legacy
  `step_actor`, `Actor::receive`, or selective receive.
- Synchronous `ask_actor_sync` cannot bypass the atomic admission path.
- System-priority supervision/monitor notifications remain runnable.
- The scheduler does not continually requeue an actor whose only pending
  work is awaiting durable admission.
- `admit_next_typed_actor_command` remains the explicit host-controlled
  method for committing the **command-only** transition, and
  `finish_admitted_typed_actor_command` still requires a separate,
  epoch-fenced completion snapshot.

The experimental isolated pure-bytecode executor (stacked PR #1720) now **requires** this opt-in before executing any admitted command; attempting to execute without it is rejected without publishing state. Successful completion does not disable the strict policy, and subsequent user commands still require atomic admission.

No change is made to ordinary actors unless a trusted host opts in. This is
not a default source-language semantic change.

## What this does **not** solve

The mailbox flag is process-local, but the opt-in is now **persisted as a
versioned, epoch-fenced policy transition** (draft #1724). Recovery restores
strict admission before publishing the actor, and older completed atomic
ordinary-actor tails are also automatically fenced. Unsupported policy versions
or missing strict atomic tails fail closed. Each opt-in consumes one durable
sequence before the first admitted user command.

This closes the process-local fallback but still does **not** provide a
general crash-safe inbox, process-kill evidence, durable incoming-message
identities, or exactly-once external effects.

A trusted host can still call lower-level runtime APIs; this feature is a
guard against *accidental* fallback, not a sandbox boundary against a
malicious host. The separate #1720 staged executor can run a bounded, pure scalar handler in an isolated VM after opt-in; this strict gate itself does not intercept external provider calls, durably identify incoming messages, or guarantee externally observable exactly-once behavior.

The next integration must:

1. Validate the versioned persisted policy and recovery gate end-to-end under exact-head CI and destructive restart tests.
2. Give incoming commands stable sender/message identities and a durable
   inbox/dedup record (with compatible historical-format migration).
3. Execute compiler-verified handlers under isolated staged state and
   prevent non-replay-safe host effects until the prepared intents commit.
4. Atomically finalize state/effects/outbox, and recover through destructive
   process-kill testing with real persistent backends.

Keep this work **draft** until exact-head tests, Clippy, formatting, and CI
pass on its stacked base PR #1686.

## Focused verification

```sh
cargo fmt --all -- --check
cargo test --locked --test strict_typed_durable_admission
cargo test --locked --test strict_pure_durable_bridge
cargo test --locked --test atomic_persistent_mailbox_admission
cargo test --locked --lib runtime::mailbox::tests
cargo test --locked
cargo clippy --all-targets -- -D warnings
```
