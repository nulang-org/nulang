# RFC 0030: Durable Effect Receipts

- **Status:** Draft
- **Tier:** Experimental
- **Author:** dporkka
- **Created:** 2026-09-16
- **Resolved:** (pending)
- **Language-version at effect:** n/a
- **Supersedes:** none
- **Superseded by:** none

## Summary

Define a storage-neutral, replay-safe contract for external effects executed by
durable Nulang actors. Each durable external effect occurrence receives a stable
logical invocation identity, persists an intent before provider execution, and
persists a terminal receipt before its result becomes authoritative durable
state. Recovery either returns the existing receipt, executes a never-started
invocation, or enters an explicit indeterminate-recovery policy. Retries preserve
the same logical invocation identity and provider idempotency key.

This RFC deliberately adds no new source syntax. It defines runtime/compiler
semantics that future effect annotations and provider adapters can use.

Tracking issue: #317.

## Motivation

Nulang already has several pieces required for durable computation:

- algebraic effects and effect rows;
- durable actors/entities/workflows;
- event journals and snapshots;
- durable timers and signals;
- retryable activities;
- activation fencing work for stale-writer prevention;
- message identity/deduplication work for effectively-once receiver commits.

Those mechanisms do not by themselves make an external side effect replay-safe.
A durable actor may crash after a provider has performed an action but before
Nulang has durably recorded the result. On recovery the behavior can reach the
same effect site again. Without a stable logical identity and receipt contract,
the runtime cannot tell whether that logical effect has never run, completed,
or is in the crash window between those states.

For example, a payment, HTTP mutation, email send, LLM request with billing,
secret rotation, process launch, or third-party API write cannot safely be
re-executed merely because the Nulang activation restarted.

The effect system already tells the compiler which effects a function may
perform. This RFC adds the missing durable runtime identity for a concrete
logical effect occurrence.

The goal is **effectively-once observation inside Nulang**, not an impossible
blanket promise of exactly-once execution in arbitrary external systems.
Exactly-once external execution is only possible when the provider participates
through idempotency, querying, transactions, or another protocol that closes the
external crash window.

## Design

### 1. Terminology

A **logical effect invocation** is one semantic occurrence of one effect
operation in one durable execution history.

An **attempt** is one provider execution attempt for that logical invocation.
Retries increment the attempt but do not create a new logical invocation.

An **intent** is the durable record proving that Nulang allocated the logical
invocation before provider execution.

A **receipt** is the durable terminal observation of the provider outcome that
Nulang is allowed to return during replay instead of invoking the provider
again.

An **indeterminate invocation** is an intent with no terminal receipt after a
crash or failover. The provider may have executed even though Nulang did not
record a terminal result.

### 2. Stable identity

The runtime must not derive durable effect identity from wall-clock time,
randomness, retry number, source line/column alone, or activation-local memory.

The logical identity is conceptually derived from:

```text
EffectInvocationId = H(
    durable_entity_or_actor_identity,
    durable_turn_or_sequence,
    semantic_effect_site_id,
    occurrence_index
)
```

The exact hash representation is implementation-defined while this RFC remains
Experimental, but all backends for the same compiled artifact must derive the
same identity.

The fields have these meanings:

- `durable_entity_or_actor_identity`: the logical durable owner, not a transient
  process pointer or machine-local activation id;
- `durable_turn_or_sequence`: the recoverable logical execution position;
- `semantic_effect_site_id`: compiler-owned identity for the concrete effect
  operation site;
- `occurrence_index`: distinguishes repeated execution of the same semantic site
  within one durable turn, such as loops.

Retries preserve all four identity inputs and therefore preserve the same
`EffectInvocationId`.

### 3. Semantic effect-site identity

The compiler must eventually emit a canonical `EffectSiteId` into semantic IR
and preserve it through MIR and supported backends.

A source line/column is not sufficient because formatting, comments, or moving
unrelated declarations would silently change durable identity. The site id must
be derived from semantic ownership plus a stable compiler-local operation
identity.

A suitable long-term shape is conceptually:

```text
EffectSiteId = SemanticId(module, owner, effect, operation, stable_local_slot)
```

The canonical representation should compose with the existing/future MIR
`SemanticId` work rather than creating a second unrelated identity system.

Bytecode/WASM/native may lower the identity to compact local indices, but the
artifact must retain enough metadata to recover the canonical semantic meaning.

### 4. Runtime data model

The first implementation slice adds storage-neutral primitives equivalent to:

```rust
pub struct EffectSiteId(/* opaque versioned bytes */);

pub struct EffectInvocationId(/* opaque versioned bytes */);

pub struct EffectIntent {
    pub format_version: u16,
    pub invocation_id: EffectInvocationId,
    pub effect_identity: EffectIdentity,
    pub request_fingerprint: RequestFingerprint,
    pub provider_idempotency_key: Option<String>,
    pub first_attempt: u32,
}

pub enum EffectOutcome {
    Succeeded(RecordedValue),
    Failed(RecordedFailure),
}

pub struct EffectReceipt {
    pub format_version: u16,
    pub invocation_id: EffectInvocationId,
    pub effect_identity: EffectIdentity,
    pub request_fingerprint: RequestFingerprint,
    pub completed_attempt: u32,
    pub outcome: EffectOutcome,
}

pub enum EffectReplayDecision {
    ExecuteNew,
    ReturnReceipt(EffectReceipt),
    RecoverIndeterminate(EffectIntent),
}
```

Names are illustrative; implementation may refine them without changing the
semantics below.

`EffectIdentity` identifies the canonical effect and operation, not a provider
implementation. A Stripe adapter and a test adapter for the same semantic
operation may use different provider metadata while preserving the Nulang
semantic identity.

### 5. Request fingerprint

Every durable external effect intent carries a deterministic request
fingerprint over the semantic input that affects provider behavior.

If recovery encounters an existing `EffectInvocationId` with a different
request fingerprint, the runtime must fail closed. Reusing one logical
invocation identity for two different provider requests is corruption or a
compiler/runtime identity bug, not a retry.

The fingerprint must not require storing raw secrets. Provider adapters should
canonicalize and hash sensitive inputs, and receipts must avoid copying secret
material into logs or inspection surfaces.

### 6. Provider idempotency key

For providers that support idempotency, Nulang derives an opaque provider key
from `EffectInvocationId` plus adapter namespace/version.

The provider key must remain stable across:

- retry;
- actor restart;
- process restart;
- node failover;
- migration;
- replay.

Changing adapter version must not silently change the idempotency key for an
already-created intent. The concrete key used for the invocation is therefore
persisted in the intent when one exists.

### 7. Effect classes

Not every effect has identical recovery semantics. Each durable external effect
operation must declare one of these runtime policies, or a future refinement
with equivalent explicitness:

#### Pure/local

No external side effect and no durable receipt required. Examples include pure
computation and process-local deterministic helpers.

#### Replayable observation

The operation may be repeated safely because it is observational and the
application accepts a fresh observation, or the result may be receipt-cached.
The runtime still needs a policy because a fresh observation may differ from the
historical observation.

#### Idempotent external

Retry is permitted with the same stable provider idempotency key.

#### Queryable external

After an indeterminate crash, the adapter can query provider state by the stable
invocation/idempotency key and synthesize the terminal receipt if the provider
completed.

#### Compensatable

The operation may participate in a saga/compensation protocol. Compensation is
not equivalent to "the original operation never happened"; both actions remain
part of durable history.

#### Non-repeatable external

The provider supplies no safe retry/query/transaction protocol. An indeterminate
recovery fails closed unless application/operator policy explicitly chooses a
resolution. Silent automatic replay is forbidden.

Unknown external effects in durable execution default to the conservative
non-repeatable/explicit-policy path until classified.

### 8. Execution protocol

For a durable external effect invocation:

1. derive the stable `EffectInvocationId`;
2. compute the request fingerprint;
3. look up existing effect state for that invocation id;
4. if a terminal receipt exists:
   - verify request fingerprint and effect identity;
   - return the recorded terminal outcome without provider execution;
5. if no intent exists:
   - persist the intent under the current activation fence;
   - only after the intent commit succeeds may provider execution begin;
6. execute or retry according to the effect policy, preserving the same
   invocation id and provider idempotency key;
7. persist the terminal receipt under the current activation fence;
8. only after the terminal receipt commit succeeds may the effect result become
   authoritative input to subsequent durable state transitions.

The runtime may optimize batching as long as crash recovery is observationally
equivalent to this ordering.

### 9. Recovery state machine

Recovery for one invocation is deterministic:

```text
(no intent, no receipt)
        |
        v
    ExecuteNew

(intent, no receipt)
        |
        v
RecoverIndeterminate
        |
        +--> retry same idempotency key
        +--> query provider
        +--> compensate
        +--> fail closed / operator resolution

(intent, terminal receipt)
        |
        v
  ReturnReceipt
```

A receipt without a compatible intent is corruption unless a persistence
migration explicitly defines a legacy representation.

A request fingerprint mismatch is always fail-closed.

### 10. Crash windows

The implementation must test these windows explicitly:

#### Crash before intent commit

No provider execution may have started. Recovery sees no intent and may create a
new invocation attempt normally.

#### Crash after intent commit but before provider call

Recovery sees an indeterminate intent. For an idempotent effect, retrying with
the same provider key is safe. For a non-repeatable effect, the policy remains
explicit because the runtime cannot generally prove the provider call never
started.

#### Crash after provider success but before receipt commit

This is the critical external crash window. Recovery must not invent a new
logical invocation. It queries/retries using the same provider idempotency key,
compensates, or fails indeterminate according to policy.

#### Crash after receipt commit but before actor resume

Recovery returns the persisted receipt without invoking the provider.

#### Crash after terminal failure receipt

Recovery returns/rethrows the recorded terminal failure unless policy explicitly
classifies it as retryable under the same logical invocation.

### 11. Retry semantics

A retry is not a new logical invocation.

`attempt` is diagnostic/provider-policy metadata. It must not affect
`EffectInvocationId` or the request fingerprint.

The runtime may impose retry budgets, deadlines, and backoff, but those policies
must survive recovery without accidentally resetting a logical invocation into a
new identity.

### 12. Interaction with activation fencing

Effect intent and receipt writes are authoritative durable writes and therefore
must use the same activation-fencing semantics as actor state.

A stale activation may not:

- create a new intent;
- increment authoritative attempt metadata;
- commit a terminal receipt;
- overwrite a receipt;
- make an effect result authoritative after its epoch has been superseded.

Provider execution itself may already have occurred before stale authority is
detected. That is another reason the external guarantee is not blanket
exactly-once. Fencing prevents stale Nulang state commits; provider idempotency or
query semantics close the provider side where possible.

### 13. Interaction with message deduplication

Message deduplication and effect receipts solve different boundaries:

- message dedup prevents reprocessing the same logical delivery as a new
  receiver commit;
- effect receipts prevent re-executing the same logical external effect as a new
  side effect.

Where one durable turn consumes a message and produces both state changes and an
effect result, persistence adapters should eventually expose one recoverable
commit protocol for the relevant inbox/dedup position, effect receipt, and actor
state transition.

This RFC does not require a universal cross-provider transaction. It requires
that Nulang's own durable records cannot contradict each other after recovery.

### 14. Interaction with workflows and activities

Workflow activities are natural first consumers of this contract because they
already represent external or long-running work with retry semantics.

The activity id becomes or contains the logical `EffectInvocationId`, and retries
preserve it. An activity adapter may map the id to a provider idempotency key.

Existing workflow behavior that blindly starts a fresh external call after
recovery should migrate toward receipt-backed execution for operations whose
duplication is observable or billable.

### 15. Interaction with algebraic effect handlers

This RFC does not make every `perform` durable. Ordinary algebraic effects remain
handler-resolved and may be purely local.

Receipt behavior applies only when all of the following are true:

- execution is in a durable context;
- the concrete effect operation is classified as externally observable and
  durable;
- the runtime/provider adapter opts into the receipt protocol.

User-defined local handlers therefore retain existing semantics unless they
explicitly cross the durable external boundary.

### 16. Observability

Actor/effect inspection may expose:

- logical invocation id;
- canonical effect/operation name;
- state: intent / executing / indeterminate / succeeded / failed;
- current or terminal attempt count;
- latency and retry counts;
- trace/correlation identifiers;
- provider adapter name/version.

It must not expose raw secrets, authorization tokens, or unredacted sensitive
request/response bodies by default.

### 17. Persistence interface

The first implementation should define backend-neutral operations resembling:

```rust
trait EffectReceiptStore {
    fn load_effect_state(
        &self,
        invocation: &EffectInvocationId,
    ) -> Result<Option<PersistedEffectState>, PersistError>;

    fn create_intent(
        &mut self,
        fence: &ActivationFence,
        intent: &EffectIntent,
    ) -> Result<(), PersistError>;

    fn commit_receipt(
        &mut self,
        fence: &ActivationFence,
        receipt: &EffectReceipt,
    ) -> Result<(), PersistError>;
}
```

`create_intent` is create-if-absent/idempotent for an identical intent and fails
on conflicting identity/fingerprint data.

`commit_receipt` is idempotent for an identical terminal receipt and fails on a
conflicting terminal receipt.

Backends must implement the fence check atomically with the authoritative write.

### 18. Initial implementation sequence

This RFC should be implemented in narrow slices:

1. storage-neutral identity/intent/receipt/replay-decision types;
2. deterministic validation and an in-memory reference store;
3. crash-window state-machine tests;
4. activation-fence integration;
5. one existing test/external effect adapter end-to-end;
6. persisted backend adapters;
7. compiler `EffectSiteId` metadata;
8. bytecode/WASM/native conformance for site identity;
9. operator/CLI inspection;
10. only then consider source-level annotations for effect policy.

No syntax should be added merely to expose an unfinished runtime contract.

## Tier Classification

**Experimental.** This RFC defines new runtime metadata and durability semantics
outside Frozen Core. The representation may change while implementation and
failure testing mature.

If the model proves stable, a later RFC may promote the semantic contract to
Stable while keeping storage codecs independently versioned.

No existing Stable or Frozen surface is removed.

## Backwards Compatibility

The first implementation is additive. Existing effects keep their current
semantics unless an operation is explicitly migrated to receipt-backed durable
execution.

Persisted actors created before effect-receipt support have no historical
receipts. The runtime must not retroactively infer that old external operations
were exactly-once. Migration applies only from the first execution version that
records intents/receipts.

If a provider adapter changes its canonical request encoding or idempotency-key
mapping, it must version that mapping and preserve the version recorded in
existing intents.

## Alternatives Considered

### 1. Re-execute all effects on recovery

Rejected. It is simple but duplicates externally observable side effects and can
create financial/security/data corruption.

### 2. Persist only the returned result

Rejected. A result-only record cannot distinguish "provider never ran" from
"provider ran but Nulang crashed before persisting the result".

### 3. Call every external effect exactly once

Rejected as an unconditional guarantee. A process can crash after the provider
commits but before Nulang observes the response. Without provider cooperation,
no local runtime can determine whether execution happened.

### 4. Use retry attempt as identity

Rejected. Retries must refer to the same logical invocation, not create new
external operations.

### 5. Use source position as durable identity

Rejected. Formatting and unrelated source movement would mutate durable
semantics.

### 6. Put effect receipts only in workflow activities

Rejected as the final model. Activities are a good first integration point, but
durable entities and actors can also cross external effect boundaries. The
primitive belongs to the durable runtime and activities should build on it.

## Open Questions

1. Which canonical hashing scheme/version should encode `EffectInvocationId`
   once the semantic fields are finalized?
2. Should terminal retryable provider failures be receipts or remain
   non-terminal attempt records until retry policy is exhausted?
3. Which existing external effect is the safest first end-to-end integration
   target for conformance tests?
4. Should replayable observations default to returning the historic receipt or
   allow an explicit "fresh observation" policy?
5. How should operator resolution of a non-repeatable indeterminate invocation
   be represented in the durable audit trail?
6. Should effect policy eventually be declared in source syntax, package
   metadata, provider adapter metadata, or some combination?

## Resolution

(To be filled on accept/reject.)
