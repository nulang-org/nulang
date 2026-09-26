# RFC 0018: Durable External Effects

- **Status:** Draft
- **Tier:** Stable runtime semantics; Experimental executor/adapters
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-10
- **Resolved:** (pending)
- **Language-version at effect:** no syntax change
- **Supersedes:** none
- **Superseded by:** none

## Summary

Define one durable execution contract for nondeterministic and externally
observable effects executed by persistent Nulang actors. Before an external
effect is executed, the runtime durably records an `EffectRequested` event
with a stable `EffectId`. On successful completion it durably records
`EffectCompleted` before exposing the result to actor code. Replay returns the
recorded result and never re-executes an already-completed effect. If recovery
finds a request without a completion, it retries with the same `EffectId`.
Adapters that support idempotency keys MUST propagate that ID; adapters that do
not are explicitly at-least-once with respect to the external system.

This RFC guarantees **exactly-once observation by the durable Nulang actor**.
It deliberately does not claim generic exactly-once execution against arbitrary
external systems, because that guarantee is impossible without cooperation
from the external system or a transaction spanning both systems.

## Motivation

Nulang already persists actor/workflow state and can suspend a bytecode
behavior while an LLM request executes. The current recovery path cannot
persist the suspended VM state for an in-flight LLM call. A workflow recovered
from the pre-suspend checkpoint therefore re-runs the step and starts a fresh
provider call. The same failure window applies to future HTTP mutations,
payments, email sends, tool calls, database mutations, and other external
operations:

```text
persistent actor
  -> external request succeeds
  -> process dies before local completion is durable
  -> actor recovers
  -> request executes again
```

For read-like operations such as an LLM completion this can produce a different
answer and duplicate cost. For mutation-like operations it can duplicate a
payment, email, deployment, or physical action.

The problem must be solved once at the effect boundary, not independently in
LLM, HTTP, workflow, and provider-specific code. Algebraic effects are already
Nulang's representation of observable operations, so they are the natural
boundary for durable execution.

## Design

### 1. Effect classes

Every runtime-handled effect operation is assigned one execution class:

```rust
pub enum EffectDurability {
    /// Pure/deterministic with respect to replay. No journal entry required.
    Deterministic,
    /// Nondeterministic result whose value must be captured for replay.
    Recorded,
    /// External operation whose request/result lifecycle must be durable.
    External,
    /// Explicitly forbidden from a deterministic persistent context.
    EphemeralOnly,
}
```

Examples of intended classification:

| Effect | Class | Notes |
|---|---|---|
| integer/string pure helpers | `Deterministic` | replay locally |
| `Clock.now_ms`, random values | `Recorded` | first result is journaled and replayed |
| `Timer.sleep` | existing durable timer path | remains runtime-internal but obeys the same replay principle |
| `Provider.ask("llm", ...)` | `External` | provider request/result journaled |
| mutating HTTP/tool operations | `External` | stable idempotency key when adapter supports it |
| unrestricted native/Python side effects | `EphemeralOnly` by default | adapter may opt into `External` only with a declared replay contract |

Effect classification belongs to runtime/provider registration metadata, not to
transient language syntax. Deprecated `LLM.ask` and future SDK helpers map to
the same provider/effect classification.

### 2. Stable effect identity

Every recorded/external effect executed in a durable actor invocation receives
an `EffectId` that is stable across replay of that invocation:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectId([u8; 32]);
```

The canonical ID is BLAKE3 over a versioned tuple:

```text
"nulang-effect-v1" ||
actor_durable_identity ||
invocation_sequence_u64_be ||
effect_ordinal_u32_be ||
effect_name_utf8 ||
input_hash_32
```

`invocation_sequence` is the durable message/behavior invocation sequence, not
a process-local scheduler counter. `effect_ordinal` is incremented in program
order within that invocation. Replay of the same invocation must therefore
compute the same ID. A different input at the same replay position changes
`input_hash` and is treated as a determinism violation rather than silently
reusing a mismatched result.

Until durable entity identity is fully separated from activation identity,
existing persistent actors may use their persisted actor ID as
`actor_durable_identity`. The virtual-actor/entity key is preferred where
available. A later actor-ID RFC may replace the identity encoding without
changing this semantic contract; the hash prefix/version prevents ambiguity.

### 3. Durable effect journal

Add an effect journal independent of VM continuation representation:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DurableEffectEvent {
    Requested {
        sequence: u64,
        effect_id: EffectId,
        effect_name: String,
        input_hash: [u8; 32],
        codec_version: u16,
        request: Vec<u8>,
    },
    Completed {
        sequence: u64,
        effect_id: EffectId,
        codec_version: u16,
        result: Vec<u8>,
    },
    Failed {
        sequence: u64,
        effect_id: EffectId,
        retryable: bool,
        error_code: String,
        error_message: String,
    },
}
```

The byte payload is a versioned durable-value encoding. It MUST NOT serialize
raw `vm::Value` pointer bits. Initial implementation may use a canonical JSON
encoding of the existing `PersistedValue` family; before Stable format
classification, complex values must move to a dedicated versioned
`DurableValue` representation that contains no process addresses or heap-local
IDs.

The persistence layer gains fallible APIs with explicit errors:

```rust
fn append_effect_event(
    &mut self,
    actor_id: u64,
    event: DurableEffectEvent,
) -> io::Result<()>;

fn read_effect_events(
    &self,
    actor_id: u64,
) -> io::Result<Vec<DurableEffectEvent>>;
```

The existing `MemoryStore`, `JsonFileStore`, and `LibsqlStore` implementations
must implement the same ordering and crash-recovery behavior before the runtime
may use durable external effects with that backend.

### 4. Write-before-execute protocol

For an `External` operation in a durable actor, the runtime executes the
following state machine:

```text
                    +------------------+
                    | compute EffectId |
                    +---------+--------+
                              |
                              v
                    +------------------+
                    | lookup journal   |
                    +--+------------+--+
                       |            |
             completed |            | missing
                       |            |
                       v            v
                 return result   persist Requested
                                      |
                                      v
                                submit external work
                                      |
                         +------------+------------+
                         |                         |
                         v                         v
                    provider result            retryable error
                         |                         |
                         v                         v
                 persist Completed          persist Failed
                         |
                         v
                    resume actor
```

Ordering requirements are load-bearing:

1. `Requested` MUST be durable before the adapter may execute the external
   operation.
2. `Completed` MUST be durable before the actor may observe the result.
3. Replay MUST consult the journal before dispatching external work.
4. A replayed `Completed` result MUST be returned without contacting the
   adapter/provider.
5. A `Requested` record with no terminal result is retried using the same
   `EffectId`, subject to retry policy.
6. A request whose recomputed name/input hash differs from the existing entry
   at that invocation/ordinal is a determinism error and stops replay.

### 5. External delivery guarantees

Nulang exposes the actual guarantee instead of collapsing several different
semantics into the phrase "exactly once":

```rust
pub enum ExternalDeliveryGuarantee {
    /// Adapter/provider accepts a caller-supplied idempotency key whose scope
    /// covers retries of the operation.
    IdempotentRetry,
    /// The operation may execute more than once externally after a crash.
    AtLeastOnce,
}
```

For `IdempotentRetry`, the adapter MUST derive or directly pass the stable
`EffectId` as the provider idempotency key. The runtime still journals the
result because provider-side deduplication does not replace deterministic
replay.

For `AtLeastOnce`, documentation and tracing MUST say that duplicate external
execution is possible in the `Requested -> external success -> crash before
Completed` window. The actor nevertheless observes one journaled terminal
result.

Adapters MUST NOT advertise exactly-once external execution unless they
participate in a transaction protocol that can prove it.

### 6. Executor boundary

Move external work scheduling behind a runtime-independent service boundary:

```rust
pub struct EffectRequest {
    pub actor_id: u64,
    pub effect_id: EffectId,
    pub name: String,
    pub payload: Vec<u8>,
    pub deadline_ms: Option<u64>,
}

pub struct EffectCompletion {
    pub actor_id: u64,
    pub effect_id: EffectId,
    pub result: Result<Vec<u8>, EffectExecutionError>,
}

pub trait EffectExecutor: Send + Sync {
    fn try_submit(&self, request: EffectRequest) -> Result<(), SubmitError>;
}
```

The in-process implementation uses bounded queues and finite concurrency.
Submission from an actor scheduler MUST be non-blocking. Queue saturation is
backpressure, not permission to allocate an unbounded queue.

`Runtime` should ultimately own a handle to an executor service rather than
LLM-specific worker threads. `Provider.ask("llm", ...)`, HTTP adapters, MCP/tool
calls, and other external integrations become executor adapters. The bounded
shared LLM executor introduced during architecture hardening is an intermediate
step toward this generic service.

### 7. Recovery

During durable actor recovery, the runtime reconstructs an effect index keyed
by `(invocation_sequence, effect_ordinal)` and `EffectId`.

- `Completed`: return the recorded result when replay reaches the effect.
- terminal `Failed`: reproduce the same failure unless policy explicitly
  schedules a durable retry.
- `Requested` only: resubmit with the same `EffectId`.
- no event: persist `Requested` and execute normally.

Recovery MUST NOT depend on serializing a live Rust future, Tokio task, native
stack, or raw VM pointer. Persisting VM continuations may improve efficiency,
but correctness comes from event/effect replay.

### 8. LLM migration

The existing LLM suspension path currently uses `__llm_ask_pending__` as a
recovery marker and can reissue a fresh request after restart. Migration occurs
in two phases:

1. Route provider calls through `EffectExecutor`, assigning a stable
   `EffectId` and journaling request/result events. Keep the existing VM
   suspension mechanism for in-process scheduling.
2. On recovery, replace "pending marker means execute a fresh LLM call" with
   journal lookup. A completed call returns its stored result; an incomplete
   request retries with the same ID.

The LLM-specific marker may then remain only as a compatibility/recovery hint
for snapshots created before this RFC is implemented and should eventually be
removed.

### 9. Other nondeterminism

`Recorded` effects use the same identity/journal lookup but do not require an
external executor. Examples include wall-clock reads and random-number draws.
The first execution records the value; replay returns it.

Persistent actors must not call an `EphemeralOnly` effect from a deterministic
region unless the programmer explicitly exits the durable boundary. The effect
checker should eventually enforce this transitively once interprocedural effect
propagation is complete.

### 10. Cancellation and timeouts

Cancellation of the local actor does not erase a durable external request.
The journal records cancellation as a terminal outcome only after the adapter
confirms whatever cancellation semantics it supports. If the provider cannot
cancel an already-submitted operation, the request remains recoverable until a
terminal result is known.

Timeout is likewise an outcome/policy decision, not proof that an external
operation did not happen. Retrying after timeout therefore uses the same
`EffectId` whenever idempotency is supported.

### 11. Observability

Every trace/span for a durable external effect includes:

- `effect.id`
- durable actor/entity identity
- invocation sequence and effect ordinal
- effect/provider name
- delivery guarantee (`idempotent_retry` or `at_least_once`)
- replayed vs executed flag
- attempt number
- terminal status

Logs MUST NOT include secrets or full sensitive request payloads by default;
use the input hash for correlation.

### 12. Implementation sequence

Implementation should be delivered in reviewable stages:

1. Add `EffectId`, `DurableEffectEvent`, canonical payload hashing, and store
   conformance tests without changing runtime behavior.
2. Add a bounded process-wide `EffectExecutor` and an in-memory test adapter.
3. Route `Provider.ask("llm", ...)` through the executor and journal successful
   request/result pairs for persistent actors.
4. Make recovery reuse `Completed` results and retry `Requested`-only entries
   with the same ID.
5. Add crash-window tests that kill/recreate a runtime after `Requested`, after
   external completion, and after `Completed` persistence.
6. Add HTTP/tool adapters only after the generic protocol passes those tests.
7. Add interprocedural deterministic-effect enforcement as a separate compiler
   change.

No existing persistence format should be declared Frozen merely to implement
this RFC. The effect journal remains Stable runtime semantics with an
Experimental physical encoding until the durable-value format is reviewed.

## Required conformance tests

At minimum:

- completed effects are never executed during replay;
- `Requested`-only effects retry with the same `EffectId`;
- changed input at the same replay ordinal is rejected as nondeterministic;
- bounded executor saturation never blocks a scheduler thread;
- failed submission does not mark an actor/effect in flight;
- an idempotency-aware fake provider observes the same key across crash retry;
- an at-least-once fake provider demonstrates/document the duplicate-execution
  window without duplicating actor-visible completion;
- Memory, JSON-file, and libSQL stores preserve event order and terminal state;
- effect payload serialization never persists raw pointer-tagged `vm::Value`
  bits.

## Tier Classification

The replay semantics for effects executed by persistent actors are **Stable**:
programs must be able to rely on a completed recorded/external effect not being
re-executed merely because the actor restarts. Executor implementations,
adapter inventory, queue defaults, and physical journal encoding remain
**Experimental** until separately stabilized.

No Frozen bytecode or wire-format change is required by this RFC.

## Backwards Compatibility

This is behaviorally tightening for durable actors. Existing programs continue
to compile. Programs that accidentally relied on a completed external effect
being executed again after recovery will instead receive the journaled result.
That behavior is considered a bug fix, not a supported compatibility surface.

Snapshots/journals created before implementation have no effect events. Their
first post-upgrade replay therefore behaves as today for the first unresolved
operation, after which the new journal establishes stable replay semantics.
Migration code must distinguish "legacy snapshot with no effect journal" from
"new journal expected but corrupt/missing" so corruption is never silently
interpreted as a first execution.

## Alternatives Considered

1. **Persist arbitrary live VM/future state and resume it exactly.** Useful as
   an optimization, but insufficient: a process can still die after the
   external system commits and before the local continuation is persisted.
2. **Make every adapter responsible for its own replay logic.** Rejected because
   it duplicates the hardest correctness logic and produces inconsistent
   guarantees across LLM, HTTP, payments, tools, and databases.
3. **Claim exactly-once external effects.** Rejected because arbitrary external
   systems cannot provide that guarantee without idempotency or a distributed
   transaction.
4. **Use only retries and provider idempotency keys without a local result
   journal.** Rejected because replay must also reproduce the exact result seen
   by actor code, and not every provider retains/retrieves prior responses.
5. **Journal only results, not requests.** Rejected because a crash between
   external execution and result persistence needs a stable pre-execution ID
   to deduplicate the retry when possible.

## Open Questions

1. Should the first physical durable-value codec be canonical JSON or a compact
   binary encoding? This does not change the semantic protocol.
2. Should `EffectId` include a source-span fingerprint for diagnostics in
   addition to invocation ordinal? It must not depend on unstable line numbers
   for identity.
3. Should queue saturation surface as a typed `Backpressure` effect error or
   automatically schedule a durable retry? Either preserves the non-blocking
   executor invariant.
4. Which first-party adapters can honestly advertise `IdempotentRetry`?

## Resolution

(To be filled on accept/reject.)
