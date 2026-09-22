# RFC 0017: Unified Runtime Primitives

- **Status:** Accepted — Phase 3 underway
- **Tier:** Experimental
- **Created:** 2026-09-11
- **Supersedes:** RFC 0004 (Draft) where it proposed that agent/workflow ergonomics must be removed rather than lowered to actors

## Summary

Nulang has one execution model: actors. Higher-level constructs such as `agent`,
`workflow`, `entity`, `organization`, and virtual actors are compositions or
specializations of actors, not independent runtime species.

The canonical semantic model contains five primitives:

1. **Actor** — identity plus computation.
2. **State** — local, durable, event-sourced, or CRDT-backed state.
3. **Message** — `send`, `ask`, `receive`, and signals.
4. **Effect** — interaction with storage, HTTP, inference, time, queues, and other resources.
5. **Capability** — authority to perform effects or move references across boundaries.

Supervision is a runtime composition built from actors, lifecycle relationships,
failure messages, links, monitors, and restart policy. Time is expressed through the
effect system. Implementations may optimize both aggressively without introducing
additional semantic species.

Everything else should lower to a composition of these primitives.

## Motivation

Nulang already lowers `agent` and `workflow` declarations into persistent actors in
HIR. The runtime, however, still carries several legacy boolean flags and individual
subsystems sometimes branch on those flags independently. That creates three risks:

- semantic drift between compiler and runtime;
- new features accidentally becoming new execution species;
- documentation making stronger guarantees than the runtime can actually enforce.

The goal of this RFC is not to remove useful surface syntax. It is to make the
runtime semantics smaller and more compositional.

RFC 0004 correctly identified the coupling problem but was never accepted. This RFC
keeps its central architectural insight—agents and workflows are not timeless core
primitives—while allowing ergonomic syntax to remain as compiler sugar so long as it
lowers to the canonical actor/effect model.

## Surface syntax and lowering

The following source constructs remain valid and ergonomic:

```text
actor        -> actor
entity       -> persistent actor, event-sourced by default
agent        -> persistent actor + inference capability + tools + memory
workflow     -> persistent actor + durable effects + time/signals + compensation
organization -> persistent actor + supervised actor hierarchy
a virtual actor/entity -> actor identity + activation/placement policy
```

A compiler implementation may preserve source-origin metadata for diagnostics and
specialized optimizations, but executable behavior must be expressible through the
five primitives above.

## Canonical actor semantics

Phase 1 introduced `primitives::ActorRole` as a compatibility view over legacy
`is_agent`, `is_workflow`, `is_organization`, and virtual-role flags. That removed
independent precedence rules, but it still modeled orthogonal properties as one
mutually exclusive role.

Phase 3 supersedes that model for new semantic code with the runtime-neutral
`actor_semantics::ActorSemantics` normalization:

- durability is transient or durable;
- activation is eager or virtual;
- source origin is actor, agent, workflow, or organization.

This means, for example, virtual activation can compose with agent/workflow origin
instead of becoming a competing runtime species. The portable bytecode layer exposes
`ActorMeta::semantics()`, so native code and compiler-only targets such as the browser
playground share the same interpretation.

`ActorRole` remains only as a compatibility view for legacy persisted fields and
format migration. New compiler/runtime code should consume normalized semantics rather
than inventing precedence rules over boolean metadata.

## Workflows

A workflow is not a separate scheduler process. It is a durable actor whose
behaviors execute journaled effects and whose state records progress.

Conceptually:

```text
workflow
  = persistent actor
  + durable state
  + effect journal
  + timers/signals
  + optional compensation
```

Workflow syntax remains because it is clearer for orchestration code, but it lowers
to actor semantics.

## Agents

An AI agent is not a separate concurrency model.

Conceptually:

```text
agent
  = actor
  + inference capability
  + tool capabilities
  + durable memory/state
```

Agent-specific runtime optimizations are allowed, but they must preserve ordinary
actor identity, messaging, supervision, durability, and placement semantics.

## Time

`Time` is an effect family, not a separate semantic primitive. The timer wheel still
uses several internal wake-message variants, and `TimeOperation` classifies those
runtime mechanisms as:

- `Sleep` — resume an explicitly sleeping computation;
- `ScheduledDelivery` — delayed actor delivery, including durable workflow timers;
- `Deadline` — receive timeout or delayed termination;
- `RetryBackoff` — wake a retry after a delay.

This keeps workflow timers, actor timers, receive deadlines, and retry sleeps from
evolving independent scheduling semantics. The timer wheel remains an implementation
detail behind the `Time` effect/runtime handler.

## Queues and mailboxes

Actor mailboxes and general-purpose queues are related but not identical:

- **mailbox** — actor-addressed message delivery and protocol semantics;
- **queue** — a named resource for fan-out, work distribution, retention, visibility
  timeouts, or cross-system integration.

A queue therefore remains a platform effect/resource adapter. It should not become
an eighth language-level execution primitive.

## Delivery guarantees

Nulang must avoid unqualified "exactly once" claims.

The runtime may provide atomicity for its own journal/state transition and may
deduplicate messages or effects using stable identifiers. That does not make an
arbitrary external side effect exactly-once.

The supported vocabulary is:

- **at-least-once** — recovery may retry an operation;
- **effectively-once with deduplication** — retries use a stable operation/message ID
  and the receiving boundary deduplicates it;
- **backend-defined** — a configured database, queue, or storage backend owns the
  guarantee.

For an external API such as payments, email, HTTP, or model inference, effectively
once behavior requires cooperation from the external system, normally an
idempotency key or equivalent transactional contract.

## Effect boundaries

Effects fall into three durability boundaries:

1. **Runtime-owned** — state/journal controlled by Nulang.
2. **Backend-owned** — database, storage, queue, or other adapter with its own
   documented semantics.
3. **External** — operations whose side effects happen outside Nulang's transactional
   boundary and may be repeated after recovery.

These distinctions must be visible in platform documentation and observability.

## Supervisors

Supervision remains a first-class runtime facility, but it is not an independent
semantic primitive. A supervisor is an actor/lifecycle pattern built from links,
monitors, failure messages, restart strategy, escalation, restart intensity, and
eventually cross-node restart/migration policy.

This keeps supervision explicit and optimizable while avoiding a second execution
model. It remains one of the principal differences between Nulang and serverless
systems that expose only per-function retry policies.

## Placement

Placement is policy attached to an actor, not a separate runtime primitive. A future
placement engine may choose local process, node, region, GPU host, edge runtime,
customer VPC, or other backend while preserving actor identity and message semantics.

## Compatibility

The normalization phases are additive:

- no source syntax is removed;
- `agent`, `workflow`, and `state_machine` lower to actors before MIR;
- `agent` and `workflow` no longer have distinct HIR declaration variants;
- compile-time-only database/signal/given metadata is filtered before HIR;
- legacy role booleans remain serialized for compatibility;
- bytecode metadata and live runtime actors expose normalized actor semantics;
- timer-wheel wake variants map to `TimeOperation` without changing timer formats;
- delivery/effect vocabulary is tightened without weakening existing execution APIs.

## Phase 2 implemented in this change

- `Actor::role()` gives live runtime actors the same canonical role interpretation as
  HIR and bytecode metadata;
- workflow event, query, signal/timer eligibility paths now consume canonical role
  semantics rather than independently reading `is_workflow`;
- internal timer wake variants map to `TimeOperation`, unifying sleep, scheduled
  delivery, deadlines, and retry backoff behind the `Time` effect family;
- tests reject conflicting live actor roles just as compiler metadata already does.

## Follow-up phases

Phase 3 introduces `ActorSemantics` as the preferred compatibility view for new
compiler/runtime code. It separates three orthogonal dimensions that legacy role
flags had conflated:

- durability: transient vs durable;
- activation: eager vs virtual;
- surface origin: actor, agent, workflow, or organization.

`ActorRole` remains as a persisted-format compatibility view until the format
migration is safe. Runtime agent detection, VM callbacks, and MIR tool collection
now consume normalized semantics rather than reading `is_agent` directly.

Remaining work:

1. Audit remaining direct role-boolean reads and keep them only where compatibility
   fields are serialized or mechanically copied; semantic decisions should use
   `ActorSemantics`.
2. Replace multiple serialized booleans with versioned semantic fields in a future
   format revision.
3. Make effect-boundary metadata visible in tracing and replay inspection.
4. Require stable operation IDs for replayable external effects at the Cloud/runtime
   boundary.
5. Standardize actor message deduplication and document local FIFO vs remote delivery
   semantics.
6. Expose placement as policy while keeping actor identity stable across node changes.
