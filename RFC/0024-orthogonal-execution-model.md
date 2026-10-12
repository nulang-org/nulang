# RFC 0024: Orthogonal Execution Model

- **Status:** Proposed — Phase 1 implemented
- **Tier:** Experimental architecture guidance; no new Stable/Frozen syntax in Phase 1
- **Created:** 2026-09-22
- **Supersedes:** RFC 0017 only where RFC 0017 states that Nulang has one execution model (actors)
- **Preserves:** RFC 0017's actor-runtime cleanup, delivery semantics, effect-boundary vocabulary, and compatibility migration work

## Summary

Nulang has one semantic core, not one execution model.

The language distinguishes three execution domains:

1. **Local computation** — ordinary lexical functions/expressions and optimized local numeric/data-parallel computation.
2. **Scoped tasks** — structured concurrent child computations whose lifetime is bounded by a lexical parent scope.
3. **Actors** — independently addressable, isolated stateful computations with mailbox, turn, supervision, and protocol semantics.

Durability and identity are orthogonal to those domains. They are not actor species.

    typed computation
      |
      +-- local computation
      +-- scoped tasks
      +-- actors
      |
      +-- effects
      +-- reference capabilities
      +-- external authority
      |
      +-- persistence: ephemeral | durable | event-sourced | replicated
      +-- identity: anonymous | scoped | runtime | stable
      +-- placement/activation policy

Actors remain a first-class and strategically important execution form. This RFC only rejects the stronger claim that every form of concurrency, durability, workflow execution, or domain abstraction must semantically be an actor.

## Motivation

Nulang's implementation already contains semantics that do not fit cleanly under an actor-only ontology:

- pure/local function execution and JIT/AOT/WASM optimization;
- algebraic effects and handlers;
- reference capabilities and typed authority;
- par expressions as an explicit independence marker;
- durable workflow history and executor-neutral workflow SDK boundaries;
- virtual activation;
- typed actor protocols;
- event-sourced and replicated state;
- immutable shared objects and data-oriented execution paths.

Treating all of these as actor specializations conflates distinct properties:

- concurrency with identity;
- persistence with process lifetime;
- activation policy with source form;
- workflow history with mailbox semantics;
- external authority with reference capability;
- implementation strategy with language semantics.

The result is pressure to add more actor roles and role flags instead of composing orthogonal properties.

## Core invariants

### 1. Local computation is not an actor

Ordinary functions, expressions, loops, vectorized kernels, and pure transformations have no mailbox or independently addressable identity.

A compiler may parallelize or vectorize them when semantics permit without creating actor identities.

### 2. Scoped tasks are not actors

A scoped task is concurrent computation with a bounded lifetime.

Its intended invariants are:

- a child cannot outlive its parent scope;
- parent completion waits for child completion unless cancellation/error terminates the scope;
- failures propagate according to one explicit structured-concurrency policy;
- child results are joined through typed values;
- actor mutable state cannot be concurrently mutated by ordinary task branches;
- ownership/reference-capability rules govern values moved into branches.

The existing par surface is the first candidate for this execution domain. It remains sequential today; Phase 2 changes lowering/runtime behavior only after these invariants are enforced.

### 3. Actors mean addressable isolation

An actor provides:

- runtime identity;
- isolated mutable state;
- mailbox/message delivery;
- serialized ordinary turns;
- typed protocol boundaries;
- supervision/failure relationships;
- optional remote placement/routing.

Actors coordinate independently evolving state. They are not required merely to achieve parallelism.

### 4. Persistence is orthogonal

Persistence semantics are a separate axis:

- ephemeral;
- durable;
- event-sourced;
- replicated, for explicit convergence models such as CRDT-backed state.

Examples:

    local function      + ephemeral
    scoped task         + ephemeral
    actor               + ephemeral
    entity              + durable
    entity              + event-sourced
    workflow            + durable history

A runtime may host a durable workflow using an actor internally. That hosting choice is not the semantic definition of a workflow.

### 5. Identity is orthogonal

Identity semantics are:

- anonymous — no independent identity;
- scoped — identity/handle only inside a structured parent scope;
- runtime — live runtime identity;
- stable — logical identity survives activation/restart/placement.

Virtual activation is actor/entity activation policy, not a mutually exclusive runtime role.

### 6. Effects, reference capabilities, and authority stay separate

Nulang keeps three different questions distinct:

    type/reference capability  -> what may this value alias/mutate/share?
    effect                     -> what operation may this computation request?
    authority                  -> what external boundary may it actually cross?

External authority remains deny-by-default and is not renamed to generic capability.

## Surface constructs

Higher-level syntax remains ergonomic, but it lowers to orthogonal semantics.

    actor
      = actor execution domain
      + runtime identity
      + ephemeral persistence by default

    entity
      = actor execution domain
      + stable identity
      + durable/event-sourced/replicated state policy

    workflow
      = durable structured computation
      + history
      + durable effects/time/signals
      + optional compensation

    agent
      = execution form chosen by its host abstraction
      + inference/tool authority
      + memory/policy libraries

    organization
      = supervision/policy/membership composition

    virtual actor/entity
      = actor/entity
      + virtual activation policy

No new source keyword is required by Phase 1.

## ActorRole compatibility migration

Current metadata exposes ActorRole variants Plain, Agent, Workflow, Organization, and Virtual and derives them from legacy booleans.

This remains valid as a compatibility discriminator while existing bytecode/runtime structures depend on those fields. It must not become the basis for future semantic features.

In particular:

- Virtual should migrate to activation metadata;
- workflow durability/history should migrate to explicit durable-computation metadata;
- agent/inference behavior should migrate to effects/authority/library metadata;
- organization should migrate to supervision/policy composition;
- actor execution semantics remain actor execution semantics independent of those origins.

Migration must be additive until persisted/wire formats have explicit versioned replacements.

## Relationship to workflows

A workflow is semantically durable structured computation.

The executor-neutral nulang-workflow crate is the preferred long-term boundary:

    workflow semantics
        |
        +-- local synchronous host
        +-- async host
        +-- actor-backed host
        +-- Nulang Cloud host

All hosts must preserve the same history, retry, idempotency, signal, timer, and compensation semantics.

Actor-backed execution is allowed and useful, but must remain an implementation strategy.

## Relationship to par

Current par implementation is an independence annotation with sequential execution.

Phase 2 target:

- preserve a distinct HIR/MIR representation instead of lowering immediately to an ordinary block;
- statically reject unsafe shared mutable captures;
- compute branch effect/authority summaries;
- execute eligible branches concurrently;
- join results deterministically;
- cancel sibling branches according to a documented failure policy;
- retain sequential fallback when a backend cannot execute concurrently, while preserving observable semantics.

No claim of parallel execution should be made until that phase lands.

## Runtime semantic vocabulary

Phase 1 introduces representation-independent enums in src/primitives.rs:

- ExecutionDomain;
- PersistenceSemantics;
- IdentitySemantics;
- ActorActivation;
- ExecutionSemantics.

They are intentionally not serialized yet.

The pre-existing RuntimePrimitive enum remains as the RFC 0017 actor-runtime subsystem vocabulary. It no longer defines the complete language ontology.

## Entity default persistence

This RFC does not change existing entity state defaults.

Before a stable 2.0 contract, a separate compatibility RFC should decide whether:

1. entities default to event-sourced state; or
2. entities default to ordinary durable state and require explicit domain events for event sourcing.

That decision is intentionally isolated because changing the default is user-visible and persistence-format-sensitive.

## Non-goals

Phase 1 does not:

- change bytecode or NUL0 formats;
- change actor scheduling;
- implement parallel par;
- add task or await syntax;
- remove ActorRole;
- change entity persistence defaults;
- change workflow persistence formats;
- remove agent/workflow/entity convenience syntax.

## Implementation plan

### Phase 1 — semantic vocabulary and documentation

- add orthogonal execution/persistence/identity enums;
- document ActorRole as a compatibility classifier;
- reframe RFC 0017's one-execution-model claim;
- update architecture/spec language to distinguish semantic form from implementation host;
- preserve all existing runtime behavior and serialized formats.

### Phase 2 — structured concurrency

- keep par distinct through HIR/MIR;
- define branch-result shape;
- enforce capture/ownership safety;
- add deterministic cancellation/failure semantics;
- implement concurrent execution on capable backends;
- add conformance tests proving sequential and concurrent hosts are semantically equivalent.

### Phase 3 — role decomposition

- migrate virtuality to activation metadata;
- migrate workflow durability/history to explicit metadata;
- move agent-specific decisions to inference/tool authority and libraries;
- remove new runtime decisions based on ActorRole;
- retain compatibility decoding for old artifacts.

### Phase 4 — durable computation unification

- bind language workflow lowering to the executor-neutral workflow semantic contract;
- expose the same history format and replay rules to local/runtime/cloud hosts;
- make durable time/signals/effects independent from actor-specific flags.

## Acceptance criteria

The orthogonal model is considered established when:

- no new semantic feature requires adding an ActorRole variant;
- par has tested scoped-concurrency semantics;
- workflow conformance tests pass across at least two independent hosts;
- virtual activation no longer depends on a mutually-exclusive role discriminator;
- behavior manifests can describe execution domain, persistence, identity, effects, and authority independently;
- documentation no longer claims that all concurrency/durability is semantically an actor.

## Compatibility

Phase 1 is additive and changes no executable semantics or stable format.

Future phases that alter source-visible behavior, entity defaults, or serialized metadata require their own compatibility gates and migration tests.
