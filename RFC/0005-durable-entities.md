# RFC 0005: Durable Entities

- **Status:** Historical draft — not accepted; universal-entity framing superseded by RFC 0024
- **Tier:** Stable
- **Author:** AI assistant review
- **Created:** 2026-07-21
- **Resolved:** (pending)
- **Language-version at effect:** 2.0 (planned)
- **Supersedes:** none
- **Superseded by:** RFC 0024 (execution-model architecture)

## Summary

> **Historical note (2026-09-25):** This draft predates RFC 0024 and must not be read as making `entity` the universal unit of computation. The current model keeps local computation, scoped tasks, actors, workflows, durability, identity, placement, effects, reference capabilities, and external authority orthogonal. `entity` remains a useful durable-state abstraction, but it does not subsume ordinary computation or durable workflows. Consult `SPEC2.md`, `docs/IMPLEMENTATION_STATUS.md`, and accepted RFCs for current semantics.

Introduce `entity` as a Stable language keyword for long-lived, durable, stateful computational identities. An `entity` is a `persistent actor` with stronger defaults and additional operations: event sourcing as the primary state model, stable identity across restarts, and migration-aware state evolution. The existing `actor` and `persistent actor` keywords remain Stable; `entity` is the recommended surface for software that must survive forever.

## Motivation

Nulang is repositioning as a durable computation language for long-lived, distributed, stateful software entities. The current surface has `actor` and `persistent actor`, plus four state models (`local`, `durable`, `event_sourced`, `crdt`). This is a solid foundation, but it does not make durability the obvious default for the dominant abstraction. Programmers must remember to write `persistent actor` and choose `event_sourced`; the result is that ephemeral-by-default code is easy to write by accident.

A dedicated `entity` keyword solves this by:

1. **Making durability the default.** An `entity` is persistent and event-sourced unless a field is explicitly marked `local`.
2. **Giving a name to the strategic unit.** The PRD and repositioning narrative talk about "software entities." The language should have a keyword that matches.
3. **Adding event sourcing as a first-class operation.** Entities can `emit EventName(args)` to append domain events to an event journal. State is a left fold over that journal.
4. **Supporting stable identity and migration.** Entities expose a stable identity (actor id / address) and support `migration` contracts for schema evolution without losing historical event journals.

## Design

### 1. `entity` keyword and syntax

An `entity` declaration mirrors an `actor` declaration but changes the default state model. Event declaration and emission use the existing `emit` expression and `event_sourced` state model (see RFC 0007 for the full event-sourcing design):

```nulang
entity BankAccount {
    state balance: Int = 0              // event_sourced by default
    state local scratch: String = ""    // must opt-in to ephemeral

    behavior deposit(amount: Int) {
        self.balance = self.balance + amount
    }

    behavior withdraw(amount: Int) {
        if amount > self.balance then
            Error("Insufficient funds")
        else {
            self.balance = self.balance - amount
            Ok(unit)
        }
    }

    behavior get_balance() {
        self.balance
    }
}
```

Rules:

- `entity` is equivalent to `persistent actor` with `event_sourced` as the default state model.
- Every state field inside an `entity` defaults to `event_sourced` unless annotated `durable`, `local`, or `crdt`.
- `emit EventName(args)` and `event_sourced` state are already implemented in the language (RFC 0007 extends them with typed `events` blocks and `apply` handlers). They compose naturally with `entity`.
- Behaviors are otherwise identical to actor behaviors: single-threaded execution, message-passing interface, effect rows in signatures.

### 2. State models in entities

| Annotation | Meaning in an entity |
|------------|----------------------|
| (none) | `event_sourced`: every assignment is recorded as an event in the journal. |
| `durable` | Snapshot + journal: state is checkpointed; journal stores only the operations needed for crash recovery. |
| `local` | Ephemeral: lost on restart. Must be explicit. |
| `crdt` | CRDT state that merges across nodes. Journal stores delta operations. |

The default change is intentional: entities are durable-first. The compiler should warn if an entity has only `local` state with no durable/event_sourced/crdt fields.

### 3. Event journal semantics

The event journal is an append-only, ordered log of events emitted by the entity. It is the source of truth for entity state.

- **Deterministic replay.** Replaying the journal through the entity's code must produce the same final state, given the same initial state and migration contracts.
- **Event payloads.** Payload values must be serializable. The capability system enforces that `lineariso`, `iso`, or `ref` capabilities cannot be stored in an event.
- **Journal persistence.** The runtime stores the journal in the configured persistence backend (memory, JSON file, SQLite, future libsql/Turso). Snapshots are taken at configurable intervals for efficiency.
- **Event visibility.** Events can be exposed as a stream via a Cloud SDK library (`nlc.events`) or a future Stable effect; they are not part of the Frozen Core.

### 4. Stable identity

Every spawned entity receives a stable actor id (`ActorId`, `u64`) that survives restarts. The runtime maps the entity's declared name plus a user-supplied or generated identity key to the same actor id after recovery. The existing `spawn ... as "name"` syntax already supports this:

```nulang
let account = spawn BankAccount { balance = 0 } as "account:alice-123"
```

If `as "name"` is omitted, the runtime assigns a fresh id as for ordinary actors. The `as` form is recommended for entities that must be recovered by name.

The `spawn ... as ...` syntax is Stable-tier because it is essential to durable identity.

### 5. Migration contracts

As entity state schemas evolve, historical event journals must remain readable. A `migration` block attached to an entity declares how to upgrade events and state from a previous schema version:

```nulang
entity BankAccount {
    version: 2

    state durable balance: Int = 0
    state durable currency: String = "USD"   // added in v2

    events
        | Deposited(amount: Int)
        | Withdrawn(amount: Int)
        | CurrencyChanged(currency: String)

    migration from 1 to 2 {
        // Rewrites historical events and upgrades snapshot state.
        // The migration is pure: it sees old events/state and produces new events/state.
        | Deposited(amount) => {
            emit Deposited(amount)
            emit CurrencyChanged("USD")
        }
        | other => other   // pass through
    }

    behavior deposit(amount: Int) { /* ... */ }
}
```

Rules:

- `version` defaults to 1. It is recorded in every snapshot and journal segment.
- `migration from V to W` is a pure function over events and snapshot state. It cannot perform effects.
- The runtime applies migrations lazily: on recovery, it replays the journal through the chain of migrations until the current version is reached.
- A missing migration for a journal version is a runtime error; the entity cannot resume.

### 6. Relationship to existing keywords

- `actor` remains Stable. It is the general concurrency primitive. Use it for ephemeral or short-lived workers.
- `persistent actor` remains Stable. It is equivalent to an `entity` whose fields default to `durable` rather than `event_sourced`.
- `entity` is Stable. It is the recommended surface for durable, long-lived domain objects.
- `agent`, `workflow`, `database` are Experimental/deprecated and should be expressed as ordinary entities importing Cloud SDK libraries.

### 7. Implementation targets

- `src/ast.rs`: Add `Decl::Entity` variant, or desugar `entity` to `Decl::Actor { persistent: true, ... }` plus an event model marker. The desugar approach is preferred because it minimizes new AST surface.
- `src/parser.rs`: Add `entity` keyword and `events` block parsing.
- `src/hir_lower.rs`: Desugar `entity` to `persistent actor` with `event_sourced` defaults and event declarations.
- `src/mir_lower.rs` / `src/mir_codegen.rs`: Generate event-journal writes for `emit`.
- `src/runtime/actor.rs`: Ensure `Actor` supports `event_sourced` state and journal replay.
- `src/runtime/persistence.rs`: Extend persistence backends to store and replay event journals per entity.
- `src/effect_checker.rs`: Add `emit` as a Stable effect (`Event.emit` or `Entity.emit`) with the entity's declared event row.
- `src/typechecker.rs`: Type-check event constructors against the entity's declared events.
- `src/stdlib.rs`: Register the `emit` effect.

### 8. Example: migration from current `persistent actor` to `entity`

Before:

```nulang
persistent actor BankAccount {
    state durable balance: Int = 0
    behavior deposit(amount: Int) { self.balance = self.balance + amount }
}
```

After (target):

```nulang
entity BankAccount {
    state balance: Int = 0          // event_sourced by default
    events | Deposited(amount: Int)

    behavior deposit(amount: Int) {
        emit Deposited(amount)
        self.balance = self.balance + amount
    }
}
```

The desugared form is equivalent to a `persistent actor` with `event_sourced balance` plus event-journal handling for `emit`.

## Tier Classification

- **Tier:** Stable.
- **Frozen Core impact:** None. `entity`, `emit`, `events`, and `migration` are outside Core.
- **Breaking change:** No. `entity` is additive. Existing `actor` and `persistent actor` programs remain valid.
- **Deprecation interaction:** This RFC reinforces the deprecation of `agent`/`workflow`/`database` (RFC 0004) by showing how those concepts are re-expressed as entities + Cloud SDK libraries.

## Backwards Compatibility

This RFC is purely additive. No existing programs break. The implementation must ensure that:

- `actor` and `persistent actor` behavior is unchanged.
- `entity` desugars to a `persistent actor` with event-sourced defaults, so the runtime changes are localized to the event-journal backend.
- Event journals use a versioned format; older journals without a version field are treated as version 1.

## Alternatives Considered

1. **Make `actor` default to durable.** Rejected because it would silently change the semantics of existing `actor` programs and make ephemeral actors harder to express.
2. **Introduce `event_sourced actor` instead of `entity`.** Rejected because the strategic repositioning needs a single keyword that names the durable-first abstraction. `entity` is shorter and matches the literature (DDD, Orleans, Akka Persistence).
3. **Keep event sourcing only as a state model (`state event_sourced x`).** Rejected because it does not surface `emit` or `events` as first-class syntax, which makes domain-driven design awkward.
4. **Promote `entity` to Frozen Core.** Rejected because the Frozen Core must remain small and sequential; entities, like actors, are Stable-tier.

## Open Questions

1. Should `emit` be a language keyword or a method/effect (`perform Entity.emit(Deposited(...))`)? A keyword is more readable; an effect is more consistent with Nulang's effect system.
2. Should event payloads support arbitrary Nulang values, or only a serializable subset (ints, floats, strings, bools, records, variants without closures/actor refs)? The capability system can enforce the latter.
3. Should migrations be attached to the entity type or live in a separate `migration` declaration? Attaching them keeps the evolution story local to the entity.
4. Should the `as "name"` form of `spawn` be promoted/validated specifically for entities? Durable identity matters most for entities, but the syntax is already general.

## Resolution

(To be filled on accept/reject.)
