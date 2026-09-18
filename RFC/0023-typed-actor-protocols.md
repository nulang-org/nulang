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

## Future phases

1. Encode behavior signatures directly in `Type::Actor.behavior` instead of relying on the annotation pre-pass.
2. Introduce a user-facing structural protocol type (`ActorRef[P]` or equivalent) for parameters, fields, collections, and public APIs.
3. Support protocol intersection and capability attenuation, e.g. `ActorRef[Readable & Observable]`.
4. Include protocol/version metadata in distributed actor references and wire negotiation.
5. Check rolling-upgrade compatibility between protocol versions.
6. Generate schema/IDL metadata from typed actor protocols for remote messaging and tooling.

## Non-goals

Phase 1 does not make every dynamic actor call statically resolvable, add a new `protocol` keyword, change runtime message dispatch, or alter the wire format. Those are separate compatibility-sensitive changes.
