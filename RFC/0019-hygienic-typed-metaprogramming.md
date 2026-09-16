# RFC 0019: Hygienic Typed Metaprogramming and Derivation

- **Status:** Draft
- **Tier:** Experimental
- **Author:** Nulang Core Team
- **Created:** 2026-09-16

## Summary

Introduce a staged metaprogramming system that increases Nulang's expressive power without adding new runtime semantics.

The design has three layers:

1. **Derivation** — declaration-oriented generation such as `@derive(Eq, Hash, Serialize)`.
2. **Hygienic syntax macros** — AST-to-AST expansion before type checking.
3. **Typed macros** — constrained post-typecheck transformations over typed syntax/HIR.

All macro output must lower to ordinary Nulang constructs and ultimately to the seven canonical runtime primitives defined by RFC 0017: Actor, State, Message, Effect, Capability, Supervisor, and Time.

Metaprogramming is therefore a compiler-extension mechanism, not a new execution model.

## Motivation

Nulang already has Hindley-Milner inference, row-polymorphic records and effects, typeclasses, reference capabilities, actors, durable state, supervision, and algebraic effects. The main remaining source of language growth is ergonomic surface syntax that requires special parser/compiler support.

Examples include query comprehensions, sagas, HTTP contracts, agent/workflow conveniences, serialization boilerplate, validation declarations, and future state-machine or protocol DSLs.

Without a metaprogramming mechanism, each new abstraction risks becoming permanent compiler surface area. That conflicts with RFC 0017's goal of making the runtime smaller and more compositional.

The objective is:

> Let libraries create language-like abstractions while keeping Nulang's semantic core small, analyzable, and stable.

## Goals

1. Preserve hygiene by default.
2. Preserve deterministic builds.
3. Preserve type/effect/capability checking after expansion.
4. Prevent macros from bypassing capability or effect rules.
5. Keep expansion inspectable in tooling and diagnostics.
6. Support common code generation without requiring unrestricted procedural macros.
7. Allow future surface features to be prototyped in libraries before becoming language syntax.
8. Avoid introducing runtime reflection or dynamic code loading as part of this RFC.

## Non-goals

This RFC does **not** add runtime `eval`, native compiler plugins, unrestricted token rewriting, compile-time network access, ambient filesystem access, macros that directly emit MIR/bytecode, or a second type system for macro code.

## Design principles

### Expansion must reduce complexity

Macro-expanded programs must consist only of ordinary language constructs accepted by the normal compiler pipeline:

```text
source
  -> parse
  -> syntax expansion
  -> type/effect/capability checking
  -> typed expansion
  -> HIR
  -> MIR
  -> bytecode/native/WASM
```

Typed expansion may inspect typed nodes, but its output must be revalidated before lowering continues.

### Hygiene is mandatory by default

Every identifier introduced by a macro receives a fresh syntax context. References written by the macro resolve in the macro definition environment unless explicitly injected from the invocation site.

The first implementation should not expose a general unhygienic capture escape hatch.

### Compile-time authority is capability constrained

Macro expansion runs in a compile-time sandbox. Initial macro code has access only to syntax construction/pattern matching, type information for typed macros, deterministic package metadata, and diagnostics emission.

It has no ambient IO, clock, randomness, network, environment-variable, or process-spawning authority.

### Macros are not privileged over the type system

Expansion cannot directly manufacture trusted typing facts. Generated code is checked exactly like handwritten code. Generated effects and capability-sensitive operations participate in ordinary inference and checking.

## Phase 1: Derivation

The first implementation should deliberately avoid general syntax macros and ship the lowest-risk, highest-value subset: derivation.

### Syntax

```nulang
@derive(Eq, Hash, Debug, Serialize)
type User = {
    id: Int,
    name: String,
}
```

A deriver receives a normalized declaration description and returns ordinary declarations, normally `impl` declarations.

Recommended initial built-ins:

- `Eq`
- `Debug`
- `Serialize`
- `Deserialize`

`Ord` and `Hash` should follow once the typeclass/stdlib contracts are settled.

Derivation succeeds only when required field/variant members satisfy the relevant typeclass obligations. Diagnostics must point to both the `@derive` invocation and the unsupported field.

## Phase 2: Hygienic syntax macros

After derivation is proven stable, add AST-level macros.

Recommended invocation form:

```nulang
retry!(3) {
    perform Http.get(url)
}
```

The explicit `!` distinguishes macro invocation from function calls without reserving many new keywords.

Macros should consume compiler-defined structural syntax nodes (`Expr`, `Pattern`, `Decl`, `TypeExpr`, `Block`, `Identifier`) rather than arbitrary token streams.

Quotation/splicing, if added, must construct structured AST nodes rather than perform textual substitution.

## Phase 3: Typed macros

Typed macros execute after initial type/effect/capability inference and before final HIR lowering.

Use cases include schema/codec generation, query-plan generation, protocol adapters, effect-aware wrappers, and strongly typed RPC/HTTP bindings.

Typed macros may inspect resolved types, typeclass constraints, effect rows, capability annotations, and declaration metadata. They may not mutate compiler symbol tables directly. They return new syntax/declarations which are inserted and checked again.

## Meta representation

Introduce an internal compiler-owned meta AST independent of runtime `Value`:

```rust
pub enum MetaNode {
    Expr(MetaExpr),
    Pattern(MetaPattern),
    Decl(MetaDecl),
    Type(MetaType),
    Ident(MetaIdent),
}

pub struct MetaIdent {
    pub text: String,
    pub context: SyntaxContext,
    pub span: Span,
}

pub struct SyntaxContext(u32);
```

`SyntaxContext` is compiler-only metadata. It has no runtime representation or bytecode cost.

The representation must preserve source spans and origin chains so diagnostics can distinguish invocation site, macro definition site, generated node, and nested expansion frames.

## Expansion model

Recommended order:

1. Parse source modules.
2. Resolve macro names/imports.
3. Expand derivations attached to declarations.
4. Expand syntax macros to a fixed point.
5. Run ordinary name resolution and type/effect/capability inference.
6. Run typed macros.
7. Re-run resolution/checking for generated nodes.
8. Lower to HIR/MIR.

The compiler must enforce maximum expansion depth, a generated-node budget per module, and direct/indirect expansion-cycle detection.

## Determinism and caching

Macro output must be a pure function of:

```text
macro definition content
+ invocation syntax
+ declared package metadata
+ compiler language edition/version
+ typed metadata where applicable
```

No hidden ambient inputs are allowed. The compiler may content-hash this tuple and cache expansions.

## Interaction with typeclasses

Derivation should generate ordinary `impl` declarations instead of creating a parallel derivation semantics.

For example:

```nulang
@derive(Eq)
type Point = { x: Int, y: Int }
```

conceptually expands to:

```nulang
impl Eq Point {
    fn eq(self: Point, other: Point) -> Bool {
        self.x == other.x && self.y == other.y
    }
}
```

Generated implementations pass through the normal typeclass/type/effect/capability checker.

## Interaction with effects and capabilities

Macros may generate effectful or capability-sensitive code, but generated code receives no exemption from checking.

If expansion generates `perform Http.get(url)`, the enclosing effect row reflects the HTTP effect exactly as if the programmer wrote it manually. Generated sends must satisfy ordinary sendability rules.

## Interaction with RFC 0017

This RFC does not add an eighth runtime primitive. Metaprogramming is entirely compile-time. Generated constructs lower to Actor, State, Message, Effect, Capability, Supervisor, and Time.

This is the architectural constraint: let the surface language grow while the runtime semantic vocabulary stays fixed.

## Interaction with RFC 0018

RFC 0018 notes that query comprehensions were introduced as language syntax partly because Nulang lacked a procedural macro system.

After this RFC is implemented, future query conveniences and similar DSLs should be evaluated for library implementation before adding new parser productions.

Existing query/saga syntax does not need to be removed. It can remain ergonomic syntax while compiler and standard-library internals progressively share expansion/lowering infrastructure.

## Tooling requirements

Add expansion inspection:

```bash
nulang --expand file.nula
nulang --expand=typed file.nula
```

The LSP should support generated-origin hover/navigation, invocation-site-first diagnostics, and optional expanded-code virtual documents. Runtime stack traces should normally map generated code back to the invocation site with an option to reveal expansion frames.

## Security

The initial system intentionally excludes arbitrary compile-time IO. This avoids dependency macros exfiltrating secrets, network/time/randomness-driven non-reproducible builds, and arbitrary native compiler plugins executing with developer privileges.

If build scripts or external generators are added later, they should use explicit capabilities and sandboxing rather than weakening this model.

## Implementation plan

### Phase 0 — Internal infrastructure

- Add `SyntaxContext` and origin metadata to compiler identifiers/nodes where needed.
- Add an `ExpansionContext` with depth and generated-node budgets.
- Add expansion tracing and deterministic diagnostics.
- Add conformance tests proving hygiene and span preservation.

### Phase 1 — Derive MVP

- Parse `@derive(...)` as a declaration attribute.
- Add an internal `Deriver` registry.
- Implement built-in `Eq`, `Debug`, `Serialize`, and `Deserialize` derivations.
- Generate ordinary `impl` declarations.
- Re-run normal checking on generated declarations.
- Add `--expand` output.

### Phase 2 — Library-defined derivation

- Stabilize the internal meta representation.
- Allow packages to export deterministic derive macros.
- Cache expansion by content hash.
- Record macro package versions in package-lock metadata.

### Phase 3 — Syntax macros

- Add explicit macro invocation syntax.
- Implement hygienic AST quote/splice support.
- Add cycle/depth/node-budget guards.
- Expose expansion through LSP virtual documents.

### Phase 4 — Typed macros

- Introduce a read-only typed meta API.
- Run typed expansion before final HIR lowering.
- Revalidate generated nodes.
- Prototype query/schema/RPC generation using typed macros.

## Acceptance criteria

1. `@derive(Eq, Debug)` works for records and variants.
2. Generated implementations pass through ordinary checking.
3. Macro-introduced names cannot accidentally capture invocation-site names.
4. Expansion is deterministic across repeated builds.
5. `nulang --expand` exposes generated code.
6. Generated-code diagnostics identify invocation site and expansion frame.
7. Macro code has no ambient filesystem/network/time/randomness authority.
8. Existing non-macro programs compile identically apart from additive compiler metadata/refactoring.

## Alternatives considered

### Rust-style token procedural macros

Rejected for the initial design because arbitrary token streams make structural validation, hygiene, tooling, and deterministic sandboxing harder than necessary.

### Lisp-style fully homoiconic source representation

Not adopted. Nulang can gain structural metaprogramming without converting its syntax into an S-expression representation.

### Compiler plugins in Rust

Rejected. Native plugins would couple extensions to compiler internals, break portability, complicate versioning, and execute with excessive host authority.

### Keep adding dedicated syntax

Rejected as the default strategy. Dedicated syntax remains justified for exceptionally fundamental concepts, but most ergonomic abstractions should first prove themselves through the macro/derivation layer.

## Open questions

1. Should user-authored macro functions be written in Nulang or a deliberately smaller compile-time subset?
2. Should derive macros inspect documentation attributes and field annotations in Phase 1?
3. Should `--expand` print canonical Nulang syntax, debug AST, or both?
4. Should typed macros see selected typeclass dictionary implementations or only unresolved obligations?
5. How should macro packages declare compatibility with compiler language editions?

## Recommendation

Implement **Phase 0 + Phase 1 first** and do not start general syntax macros until derivation, hygiene metadata, source mapping, and deterministic expansion are proven with production-quality tests.

The objective is not unrestricted compile-time programmability. It is one high-leverage extension mechanism that prevents future ergonomic features from permanently expanding Nulang's semantic core.
