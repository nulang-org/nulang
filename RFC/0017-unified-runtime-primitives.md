# RFC 0017: Unified Runtime Primitives

- **Status:** Accepted — Phase 1
- **Tier:** Experimental
- **Created:** 2026-09-11

## Summary

Nulang has one execution model: actors. Higher-level constructs such as `agent`,
`workflow`, `entity`, `organization`, and virtual actors are compositions or
specializations of actors, not independent runtime species.

The canonical semantic model contains seven primitives:

1. **Actor** — identity plus computation.
2. **State** — local, durable, event-sourced, or CRDT-backed state.
3. **Message** — `send`, `ask`, `receive`, and signals.
4. **Effect** — interaction with storage, HTTP, inference, queues, and other resources.
5. **Capability** — authority to perform effects or move references across boundaries.
6. **Supervisor** — lifecycle, failure containment, restart policy, links, and monitors.
7. **Time** — sleep, timer, deadline, and recurring schedule semantics.

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

Phase 1 introduces `primitives::ActorRole` as the single compatibility view over
legacy `is_agent`, `is_workflow`, `is_organization`, and virtual-role flags.

New compiler/runtime code should use the canonical role instead of inventing its
own precedence rules over boolean metadata. Conflicting specialized roles are an
error.

A later phase may replace the booleans in HIR/MIR/bytecode/runtime metadata with a
single serialized role enum once all consumers have migrated.

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

Phase 1 is additive:

- no source syntax is removed;
- `agent` and `workflow` continue to lower to actors;
- legacy role booleans remain serialized for compatibility;
- `ActorRole` provides one canonical interpretation of those flags;
- delivery/effect vocabulary is tightened without changing current execution code.

## Follow-up phases

1. Migrate HIR/MIR/bytecode/runtime consumers from direct role booleans to
   `ActorRole`.
2. Replace multiple serialized booleans with a versioned role enum.
3. Make effect-boundary metadata visible in tracing and replay inspection.
4. Require stable operation IDs for replayable external effects.
5. Standardize actor message deduplication and document local FIFO vs remote delivery
   semantics.
6. Expose placement as policy while keeping actor identity stable across node changes.
