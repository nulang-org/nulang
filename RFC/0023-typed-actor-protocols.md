# RFC 0023: Typed Actor Protocols

- **Status:** Draft — Phase 1 implemented in this branch
- **Tier:** Experimental
- **Created:** 2026-09-16

## Summary

Nulang actor references should carry enough static protocol information for the compiler to reject messages that the target actor cannot handle. The first phase derives that protocol from existing actor `behavior` declarations and feeds it into the existing Hindley-Milner typechecker through ordinary type annotations.

No new source syntax is required for Phase 1.

## Motivation

Before this RFC, `send` and `ask` only required their receiver to unify with `Type::Actor`. The behavior name was not checked, send arguments were inferred independently of the handler signature, and `ask` returned a fresh unconstrained type variable.

That makes these programs type-check even though their actor protocol is invalid:

```nulang
actor Counter {
  behavior add(x: Int) { nil }
}

let c = spawn Counter {} in {
  send c typo()
  send c add("not an int")
}
```

It also means a query such as `ask c get()` does not carry the return type of `get` unless later context happens to constrain it.

## Phase 1 design

The compiler builds a protocol table from statically known actor declarations:

```text
ActorProtocol
  behavior name
    parameter types
    return type, when known
```

Known calls are then enriched before ordinary HM inference:

1. Resolve a receiver to a known actor declaration when possible (`spawn Counter`, a local bound to that spawn, `self`, or a virtual actor `Grain("Counter", key)`).
2. Require the behavior name to exist.
3. Require exact arity.
4. Wrap annotated behavior arguments in `Expr::TypeAnnotate` using the declared parameter types.
5. Wrap `ask` in `Expr::TypeAnnotate` when the behavior return type is known.
6. Delegate all actual unification and diagnostics for argument/return mismatches to the existing HM checker.

This is deliberately not a second type system.

## Conservative return inference

A behavior's explicit return annotation is authoritative. When it is absent, Phase 1 only infers obviously safe shapes such as:

- literals;
- direct `self.state_field` reads with a declared field type;
- the last expression of a block;
- branches with the same statically obvious type;
- simple tuples/arrays and primitive operators.

If the compiler cannot establish the return type cheaply and safely, it leaves the `ask` result unconstrained exactly as before.

## Compatibility

Dynamic actor references remain permissive in Phase 1. For example, a function parameter whose concrete actor protocol is unknown continues to use the existing `Actor` constraint rather than being rejected.

This gives Nulang an incremental migration path: known actors become safer immediately without requiring every existing generic actor API to adopt protocol parameters in one release.

## Phase 2: structural actor references

Phase 2 adds an explicit compile-time-only structural reference type:

```nulang
ActorRef[{ get: () -> Int, add: Int -> Unit }]
```

A concrete actor advertises a closed record of its declared behavior signatures in
`Type::Actor.behavior`. Passing that actor where an `ActorRef[P]` is required
checks that every behavior in `P` exists with a compatible parameter, return,
effect, and capability signature. Extra concrete behaviors are permitted, so a
concrete actor can be attenuated to a smaller required interface.

Calls through `ActorRef[P]` are checked directly from `P`, which makes actor
parameters and public APIs statically safe even when the concrete actor identity
is intentionally hidden. Behavior signatures with omitted parameter or return
annotations are not allowed to satisfy a typed structural requirement; Phase 1
direct-call compatibility remains unchanged.

ActorRef behavior signatures use an explicit **message argument-pack**
interpretation for the function parameter position: `() -> R` means zero
message arguments, `T -> R` means one argument, and `(A, B) -> R` means two
arguments. Concrete actor protocols preserve the declared parameter vector as a
pack, so a behavior declared as `behavior f(pair: (A, B))` is not confused
with `behavior f(a: A, b: B)`. The current surface therefore fails closed for
the uncommon case where a public structural protocol must name one tuple-valued
message argument; a future dedicated behavior-signature syntax can make that
case explicit without reintroducing arity ambiguity.

`ActorRef[P]` and concrete actor protocol metadata are erased at the AST→HIR
boundary to the deterministic runtime actor shape `Actor[Unit, Unit]`. Erasure
is recursive across function signatures, containers, aliases, event schemas,
FFI signatures, and database schemas; inferred function return types are erased
before becoming HIR temporaries as well. This keeps protocol metadata available
for type checking and semantic analysis while preventing it from entering MIR,
bytecode, persistence, or NUL0 wire-facing artifacts.

This phase does not change runtime actor values or any stable format. Structural
subtyping between two already-abstract `ActorRef` values and distributed
protocol fingerprints remain follow-up work.

## Phase 3: abstract-reference attenuation

Already-abstract actor references support directional width attenuation. A value
of type `ActorRef[P]` may flow to `ActorRef[Q]` when every behavior required
by `Q` exists in `P` with a compatible argument-pack, return, effect, and
capability signature. This is checked at value-to-expected-type boundaries
(function arguments, explicit annotations, let annotations, and declared
returns) rather than by making ordinary HM unification asymmetric.

The reverse is rejected: a reference cannot be widened to claim behaviors it
does not expose. This makes protocol narrowing a capability attenuation
operation rather than an unchecked cast.

## Future phases

1. Add explicit protocol intersection/composition syntax for reusable named capabilities.
2. Introduce a user-facing structural protocol type (`ActorRef[P]` or equivalent) for parameters, fields, collections, and public APIs.
3. Support protocol intersection and capability attenuation, e.g. `ActorRef[Readable & Observable]`.
4. Include protocol/version metadata in distributed actor references and wire negotiation.
5. Check rolling-upgrade compatibility between protocol versions.
6. Generate schema/IDL metadata from typed actor protocols for remote messaging and tooling.

## Non-goals

Phase 1 does not make every dynamic actor call statically resolvable, add a new `protocol` keyword, change runtime message dispatch, or alter the wire format. Those are separate compatibility-sensitive changes.