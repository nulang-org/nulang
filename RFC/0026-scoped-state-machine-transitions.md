# RFC 0026: State-Scoped Machine Transitions and Pure Guards

- **Status:** Draft — not accepted
- **Tier:** Stable (proposed); new grammar is not approved until acceptance
- **Created:** 2026-11-19
- **Resolved:** Pending
- **Language-version at effect:** To be assigned upon acceptance
- **Supersedes:** None

## Summary

Extend existing state-machine actor sugar with optional source states and
effect-free guards, and expose an inspectable structural graph for reachability
and shortest event paths. No new VM opcodes, runtime scheduler or persistence
contract are required.

## Motivation

The existing machine syntax permits any event from any state. This allows
business and protocol flows to change state through transitions that should be
ineligible. The compiler can reject undeclared targets but cannot restrict
transitions to their source state.

## Proposed syntax

```nulang
state_machine Order {
    state Pending
    state Paid
    event pay(amount: Int) from Pending: Paid if amount > 0
    event reset: Pending
}
```

Legacy events without a source remain legal in every state. A source-scoped
event is ignored when its source differs from the current state. A guard
evaluates only event parameters through a closed set of pure expression forms;
function calls, side effects, external reads, and mutation are rejected.
The existing entry/exit hook semantics apply only to accepted transitions,
including self-transitions. Rejected transitions leave state unchanged and
return nil, following the old event-handler return convention.

For this initial proposal event names remain unique per machine. Multiple
source states for the same event name, hierarchical machines, parallel regions
and delayed transitions are explicitly deferred.

## Compiler and tooling

- Store optional source-state and guard metadata in the AST.
- Reject unknown source/target states and duplicate event declarations.
- Require boolean guards via normal type checking of their generated conditional.
- Desugar accepted transitions into ordinary actor behaviors, preserving the
  original actor runtime, supervision, and effect checker.
- Provide an experimental structural analysis API producing graph edges,
  potentially unreachable states, and shortest event paths.
- Treat every guard as satisfiable during graph traversal: reachability is an
  overapproximation and paths do not prove a runtime guard will permit transit.

## Safety and durability

- Guards cannot perform external IO or mutate state. This keeps transition
  eligibility deterministic for identical state and event inputs.
- Graph analysis is static and performs no user code execution.
- Persistent state machines remain subject to the existing durable transition
  contract; this RFC does not confer atomic external effects or exactly-once
  delivery and introduces no new durable state-machine runtime.

## Tier classification and backward compatibility

Only optional syntax is added. Existing state-machine sources retain the same
global event semantics, state layout, return value and hooks. The proposed
extensions require language governance approval before release into the
Stable surface. Syntax tests, lowering tests, formatter round-trips,
type-checker tests and graph-analysis tests are required.

## Alternatives considered

- Embed the XState JavaScript runtime: rejected due to language/runtime mismatch.
- Introduce a second state-machine scheduler: rejected due to divergent
  durability, effects and actor semantics.
- Add full hierarchical/parallel statecharts immediately: deferred until
  formal microstep/event-ordering and conflict semantics are specified.

## Open questions

1. Should repeated event names be grouped into typed, ordered dispatch
   transitions in a later version?
2. Should invalid source-state events return a typed error instead of nil?
3. What machine-specific metadata belongs in the Behavior Manifest?
4. What formal guarantees should be established before hierarchical machines?

## Resolution

Pending review and language-steward acceptance.
