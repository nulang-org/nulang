# Durable Execution Guarantees

This document defines the claim boundary for Nulang durable external effects.
It is intentionally narrower than "exactly once".

The executable contract spans:

- `src/semantic_identity.rs` — compiler-owned static effect-site identity;
- `src/durable_effect.rs` — effect state machine and delivery semantics;
- `src/durable_effect_runtime.rs` — storage-neutral prepare/recover/complete coordinator;
- `src/runtime/persistence.rs` — atomic transitions, activation fencing, and recovery lookup;
- `src/durable_effect_persistence.rs` — versioned persisted effect records.

The end-to-end deterministic release gate is
`tests/durable_effect_failure_matrix.rs`.

## Workflow creation store boundary

A workflow start must atomically persist `WorkflowStarted` and the initial
snapshot in one `DurableTransition`. This is implemented for stores that
support `commit_transition` (currently the in-memory and libSQL/SQLite
implementations). Unsupported stores return `Unsupported` before publishing
a workflow actor; no fallback to separate start-event and snapshot writes is
allowed on the strict atomic path.

**CLI compatibility:** the default JSON-file store does not implement atomic
transitions. The CLI now provides `--store sqlite:<path>` (or
`NULANG_STORE_PATH=sqlite:<path>`) for workflow execution and rejects missing
or unsupported workflow store selections. This is an explicit opt-in rather
than a silent store replacement: existing JSON snapshots are legacy histories
and are not automatically upgraded to atomic SQLite records. Production
migration/backup guidance remains required before converting existing users.

## Atomic workflow failure boundary (draft implementation)

The bytecode workflow step path now admits user commands using the fenced
`DurableTransition` tail and records `StepFailed` together with the final
durable actor snapshot in **one** transaction. Failure to persist the terminal
result must stop saga compensation and discard the uncommitted live activation.
Compensation records remain subsequent commits: an arbitrary remote
compensation cannot be made exactly-once by a local transaction.

Signal, timer-sleep, receive-wait, and LLM continuation paths use the same
atomic terminal success/failure helpers. A suspended command retains its
activation until its final result has committed.

**Not yet fully covered:** internal workflow behaviors, the legacy
suspension-marker snapshot update, compensation-retry recovery after an
ambiguous external effect, and non-workflow event-source writes have
separate correctness surfaces. These require their own fault-injection and
restart tests. This draft does not claim end-to-end exactly-once behavior.

## Saga compensation crash boundaries (draft)

Compensation must not be treated as a normal replayable workflow step.
Before executing a compensation body, the runtime records
`SagaCompensationStarted` through an atomic, fenced transition. After the
body succeeds, a separate `SagaCompensated` transition records completion.

The `Runtime::saga_compensation_recovery(actor_id)` inspection API reads the
full durable workflow journal and reports, in reverse completed-step order:

| State | Durable evidence | Safe response |
|---|---|---|
| `NotStarted` | No start or completion record | Eligible for explicit, controlled dispatch |
| `OutcomeUnknown` | Started but no later completed receipt | Block automatic retry; reconcile with external system |
| `Completed` | Terminal compensation receipt committed | Do not execute again |

A crash after start-intent commit and before the external call can produce the
same durable state as a crash after the external call and before receipt.
Therefore `OutcomeUnknown` is intentionally conservative. Recovery exposes
the work for operator reconciliation but does **not** automatically redispatch
it. Nor does it claim exactly-once external mutations. Durable effect IDs,
provider idempotency and a receipt/reconciliation API are needed for safe
automatic retries.

Historical `SagaCompensated` records without a corresponding start-intent
remain readable, and still mean completed. This initial implementation uses
per-step names as inherited from the existing saga journal; independent repeat
attempts of the same named step require a future per-attempt identity model.
The existing bytecode-module recovery prerequisite still applies when
executing compensation expressions after a process restart.

## Verified compensation receipt reconciliation (draft)

A new compensation intent carries an idempotency key derived using
`DurableEffectId::derive` from the actor ID, the durable `StepFailed` sequence,
the corresponding `StepCompleted` sequence, and the compensation step name.
This makes the key stable across restarts and journal-tail advancement, but
different for distinct failure attempts.

The `Runtime::reconcile_saga_compensation` API requires a
`SagaCompensationReceipt` containing this exact operation ID, a provider
identifier and an external receipt reference, **plus** a
`SagaCompensationReceiptVerifier` implemented by a trusted provider adapter.
The verifier must independently query or verify the provider and establish
that the receipt proves completion **for the supplied operation ID**.

Only after successful verification does Nulang commit
`SagaCompensationVerified` as a fenced atomic workflow event and classify
that compensation as `Completed`. Invalid IDs, missing provider evidence,
failed verification, unsupported legacy intent identities, stale fencing
epochs, or duplicate reconciliation all fail without publishing the receipt.

This is a trusted-verifier interface, **not** a built-in connection to any
provider, signature-validation engine, or automatic refund/retry system. Merely
constructing a receipt or providing a verifier that unconditionally returns
success is not trustworthy evidence. Operators must install a provider adapter
whose authorization, provenance and receipt-validation behavior have been
reviewed. The runtime intentionally does not automatically retry work in
`OutcomeUnknown` state.

Existing `SagaCompensationStarted` journal entries without an operation ID
are still readable, but cannot be externally reconciled through this new
interface. The separate local `SagaCompensated` marker continues to mean that
the compensation body returned successfully; it does not independently prove
that the remote provider applied exactly one mutation.

## What Nulang can guarantee locally

For one logical durable external effect, the runtime contract is:

1. derive the static call site from compiler-owned semantic identity;
2. combine it with replay-stable execution identity to derive one `DurableEffectId`;
3. persist a `Prepared` record atomically before provider execution;
4. bind the record to the exact request digest and full effect specification;
5. reuse the same operation ID on every recovery attempt;
6. persist the first terminal result as `Completed`;
7. replay a completed result without redispatching the provider;
8. reject request or specification drift before dispatch;
9. reject stale activation epochs at the persistence boundary;
10. model compensation as another explicit durable effect rather than pretending
    the original external mutation can be rolled back.

A Nulang-local journal or transaction cannot prove that an arbitrary remote
system executed an operation exactly once. A process can always fail after the
remote side commits and before Nulang records the receipt.

## Crash-window matrix

| Crash point | Durable state after restart | Required recovery | External observation |
|---|---|---|---|
| Before intent commit | No effect record | Start a logical attempt only if the owning durable turn was not committed | No provider call should have occurred |
| After intent, before provider | `Prepared` | Retry the same semantic operation ID | One call if the first process never reached the provider |
| After provider commit, before receipt | `Prepared` | Retry with the same operation ID | May duplicate under at-least-once; effectively-once requires provider/backend deduplication |
| After receipt, before actor/workflow resume | `Completed` | Return the recorded result | Provider must not be called again |
| Replay request/spec differs | `Prepared` or `Completed` | Fail closed | Provider must not be called |
| Backend owns semantics | Backend-defined | Delegate to the backend contract | Nulang must not strengthen the guarantee |

## Delivery semantics

### `AtLeastOnce`

Recovery may execute the dependency again. Duplicate external mutation is
permitted by the contract. Callers must use an idempotent operation, explicit
deduplication, or accept duplicates.

### `EffectivelyOnceWithDeduplication`

Nulang retries with the exact same stable operation ID. The external dependency
must actually honor that key (or provide an equivalent query/dedup mechanism).
Nulang can then observe one logical outcome across retries, but this is not a
blanket exactly-once guarantee for an uncooperative provider.

### `BackendDefined`

The configured backend owns retry, deduplication, and commit semantics. Nulang
delegates rather than inferring a stronger guarantee.

## What is not yet a production-wide guarantee

Passing this gate does **not** prove that every `perform`, workflow activity,
LLM call, HTTP request, or actor turn is wired through the durable coordinator.
Each adapter is production-ready only after its dispatch path consumes the
compiler-produced effect-site metadata and uses the same persisted operation ID.

A production external-effect adapter should have all of the following:

- compiler/runtime-stable semantic effect-site identity;
- validated bytecode/NBC effect-site metadata;
- intent persistence before dispatch;
- request and specification validation on recovery;
- a stable provider idempotency/deduplication key where required;
- a terminal receipt that replays without redispatch;
- activation fencing so stale owners cannot publish late results;
- atomic integration with the owning actor/workflow turn where its result
  affects durable state or inbox deduplication;
- trace metadata containing effect boundary, delivery semantics, and stable
  operation identity;
- deterministic crash-window tests;
- provider-specific timeout, throttling, response-loss, and failover tests.

## Release gate

Run:

```bash
bash scripts/test-durability-guarantees.sh
```

The gate is deterministic and performs no public network calls, sleeps, or
wall-clock assertions. It covers semantic-site artifact propagation, storage
recovery, crash-window behavior, and the lower-level durable-effect contracts.

## Competitive benchmark contract

Correctness comes before throughput. Once the same production adapter path uses
this contract, benchmark Nulang against durable-workflow systems with an
equivalent workload:

1. start a durable job;
2. perform a provider mutation with an idempotency key;
3. kill the process after provider commit but before local receipt commit;
4. restart on a fresh process;
5. complete the job;
6. assert external mutation count, recovered result, time to recovery, and
   steady-state overhead.

Report semantics separately from speed. A system that completes faster by
allowing duplicate mutation is not equivalent to one that closes the crash
window through deduplication.
