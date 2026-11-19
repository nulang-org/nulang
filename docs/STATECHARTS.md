# Experimental state-machine transitions

The first XState-inspired statechart milestone extends the existing NuLang
state-machine actor declaration without introducing a separate executor.
This implementation is paired with draft RFC 0026; the new syntax is not yet
ratified as Stable.

## Event semantics

```nulang
state_machine Order {
    state Pending
    state Paid

    event pay(amount: Int) from Pending: Paid if amount > 0
    event reset: Pending
}
```

The first declared state is initial. The legacy event form without a source
is accepted in every state. An event with a source only transitions when that
state is active. The optional guard must evaluate to Bool and may contain
literals, event-parameter references, field reads, arithmetic, comparisons,
and Boolean operations. Calls, mutations, external effects, and variable
references outside the event parameters are rejected.

If source or guard disallows a transition, it does nothing and returns nil;
entry/exit hooks are not called. Accepted self-transitions retain existing
exit and entry semantics. Event names must remain unique within a machine.
This proposal does not yet implement nested states, parallel regions,
delays, repeated event names with distinct source states, or durable machines
as a separate execution type.

## Structural analysis

The Rust API `nulang::statechart::analyze_machine(&decl)` accepts an
already-parsed `Decl::StateMachine` and returns its initial state, state
list, unreachable states, one shortest event sequence for each reachable
state, and a Mermaid `stateDiagram-v2` string.

The analysis treats all guards as possibly true. A reachable state might
remain unreachable at runtime for particular guard values; generated paths
are graph witnesses, not executable tests or formal proofs of feasibility.
An unreachable state found by this overapproximation is structurally
unreachable under any guard outcome.

The static API introduces no runtime IO and does not run machine code.
Conformance tests exercise parser validation, effect-free guards, type
checking, formatting, AST lowering, graph traversal, and actual actor
execution.

## Deferred work

Future versions may add event-family dispatch, hierarchical states, model-based
test generation with guard fixtures, typed invalid-event results, machine
manifest metadata, timer-triggered transitions, and inspector integration.
These require separate semantic and compatibility review.
