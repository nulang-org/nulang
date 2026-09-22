# RFC 0017: Unified Runtime Primitives

- **Status:** Accepted — Phase 2 underway
- **Tier:** Experimental
- **Created:** 2026-09-11
- **Supersedes:** RFC 0004 (Draft) where it proposed that agent/workflow ergonomics must be removed rather than lowered to actors
- **Partially superseded by:** RFC 0024 for the language-wide claim that actors are Nulang's only execution model

## Summary

This RFC originally described actors as Nulang's single execution model. RFC 0024
refines that claim: **the actor runtime has one canonical actor object model**, while
the language has multiple orthogonal execution domains (local computation, scoped
tasks, and actors). Higher-level actor-backed constructs should still avoid becoming
independent runtime species.

The canonical actor-runtime vocabulary contains eight primitives:

1. **Actor** — independently addressable isolated computation.
2. **State** — local, durable, event-sourced, or CRDT-backed state.
3. **Message** — `send`, `ask`, `receive`, and signals.
4. **Effect** — a requested interaction such as storage, HTTP, inference, or queues.
5. **Reference capability** — aliasing, mutation, ownership, and sendability constraints.
6. **Authority** — permission to cross an external host/security boundary.
7. **Supervisor** — lifecycle, failure containment, restart policy, links, and monitors.
8. **Time** — sleep, timer, deadline, and recurring schedule semantics.

Actor-backed higher-level forms should lower to compositions of these primitives.
Local computation and scoped tasks are defined separately by RFC 0024 and do not
need an actor identity merely to execute or run concurrently.

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
seven primitives above.

## Canonical actor role

Phase 1 introduced `primitives::ActorRole` as the single compatibility view over
legacy `is_agent`, `is_workflow`, `is_organization`, and virtual-role flags.

Phase 2 extends that same role interpretation to live runtime actors and migrates the
workflow runtime subsystem to consume `Actor::role()` instead of reading
`is_workflow` directly. HIR, serialized `ActorMeta`, and live runtime actors therefore
share one conflict rule and one semantic vocabulary while the legacy fields remain in
place for compatibility.

New compiler/runtime code should use the canonical role instead of inventing its
own precedence rules over boolean metadata. Conflicting specialized roles are an
error.

A later phase may replace the booleans in HIR/MIR/bytecode/runtime metadata with a
single serialized role enum once all consumers have migrated.

## Workflows

The current language/runtime implementation hosts a workflow as a durable actor whose
behaviors execute journaled effects and whose state records progress. RFC 0024
separates that hosting strategy from the semantic definition: a workflow is durable
structured computation, and an actor-backed host is one valid executor.

Conceptually:

```text
workflow
  = persistent actor
  + durable state
  + effect journal
  + timers/signals
  + optional compensation
```

Workflow syntax remains because it is clearer for orchestration code. Current
lowering remains actor-backed for compatibility; executor-neutral workflow semantics
may also be hosted without an actor as long as history/replay behavior is identical.

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

`Time` is one semantic primitive even though the timer wheel uses several internal
wake-message variants. Phase 2 introduces `TimeOperation` to classify those runtime
mechanisms as:

- `Sleep` — resume an explicitly sleeping computation;
- `ScheduledDelivery` — delayed actor delivery, including durable workflow timers;
- `Deadline` — receive timeout or delayed termination;
- `RetryBackoff` — wake a retry after a delay.

This keeps workflow timers, actor timers, receive deadlines, and retry sleeps from
evolving independent scheduling semantics. The timer wheel remains an implementation
detail behind the `Time` primitive.

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

Supervision remains a first-class primitive rather than being reduced to generic
retries. Supervisors own failure relationships: restart strategy, escalation,
restart intensity, links, monitors, and eventually cross-node restart/migration
policy.

This is one of the principal differences between Nulang and serverless systems that
only expose retry policies on individual functions or jobs.

## Placement

Placement is policy attached to an actor, not a separate runtime primitive. A future
placement engine may choose local process, node, region, GPU host, edge runtime,
customer VPC, or other backend while preserving actor identity and message semantics.

## Compatibility

Phases 1 and 2 are additive:

- no source syntax is removed;
- `agent` and `workflow` continue to lower to actors;
- legacy role booleans remain serialized for compatibility;
- HIR, bytecode metadata, and live runtime actors share `ActorRole` interpretation;
- the workflow subsystem consumes canonical roles without changing persisted state;
- timer-wheel wake variants map to `TimeOperation` without changing timer formats;
- delivery/effect vocabulary is tightened without weakening existing execution APIs.

## Phase 2 implemented in this change

- `Actor::role()` gives live runtime actors the same canonical role interpretation as
  HIR and bytecode metadata;
- workflow event, query, signal/timer eligibility paths now consume canonical role
  semantics rather than independently reading `is_workflow`;
- internal timer wake variants map to `TimeOperation`, unifying sleep, scheduled
  delivery, deadlines, and retry backoff under the `Time` primitive;
- tests reject conflicting live actor roles just as compiler metadata already does.

## Follow-up phases

1. Continue migrating remaining direct role-boolean consumers (agent, supervisor,
   recovery, and distribution paths) to `ActorRole`.
2. Replace multiple serialized booleans with a versioned role enum in a future format
   revision.
3. Make effect-boundary metadata visible in tracing and replay inspection.
4. Require stable operation IDs for replayable external effects at the Cloud/runtime
   boundary.
5. Standardize actor message deduplication and document local FIFO vs remote delivery
   semantics.
6. Expose placement as policy while keeping actor identity stable across node changes.
