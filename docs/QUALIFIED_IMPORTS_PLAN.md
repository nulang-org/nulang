# Qualified imports: implementation contract (draft)

Status: **experimental syntax AST/formatter support, not executable qualified imports**. The parser records `import path as alias` and the formatter preserves it, but the resolver and direct typechecker deliberately reject aliased imports until module-qualified symbol lookup exists end-to-end. No qualified-import program is supported yet.

## Why this work precedes aliases

Current `import stdlib::list` flattens declarations into the current module. Two imports exporting the same short name can overwrite each other or collide with local symbols. Resolver PR #1627 begins the fail-closed provenance boundary. Its follow-up must treat *all named declarations*, including module-level values, `given`, signals, workflows, and classes, as potential collisions, not only functions and types. Otherwise syntax-only aliases would create an illusion of isolation.

This preparatory branch adds that resolver coverage while preserving existing import syntax and default behavior for nonconflicting imports.

## Proposed source syntax

```nulang
import stdlib::list as list
import stdlib::set as set

let filtered = list::filter(numbers, is_positive)
```

The `import ... as ...` declarations above now parse as AST and round-trip through the formatter, but are **deliberately rejected during semantic resolution**. The `alias::function` expression still requires future parser/name-lookup work. Existing `import stdlib::list` remains a legacy unqualified import for Nulang 1.x. Do not silently change its meaning without a versioned migration.

## Resolver symbol-index foundation

The experimental resolver helper `QualifiedImportIndex` now records each
module alias against its canonical source path. `index_qualified_imports`
can inspect direct aliased imports in a parsed source file without flattening
exports into the importing module. Only declarations marked explicitly
`public` are currently discoverable (functions, type aliases, record types,
and variant types), and same-name overloads retain their separate entries.
Missing aliases, private names, and conflicting alias origins fail closed.
The `resolve_unique` helper now also requires an explicit lexical symbol
kind (function versus type), rejects wrong-kind exports, and refuses to
silently pick between overloaded candidates. It returns an **owned**
`QualifiedSymbol` with canonical source identity and no local alias.
Type-directed overload selection is still unimplemented.

**This index is not yet consumed by the production typechecker, HIR/MIR,
bytecode, WASM, native backends, or LSP.** The ordinary compile path still
rejects alias imports. Support for nested reexports, visibility on other
declaration types, constructors, and signature-driven overload selection belongs
in subsequent changes. Filesystem paths are resolver metadata only and
must not be written into runtime type identifiers or serialized artifacts.

## Required semantic invariants

1. **Lexical namespaces, not string-prefix collisions.** Bind `list` to a resolved module identity in the resolver. Qualifying a reference must select that module's exported declaration, not an arbitrary declaration whose textual name begins with `list::`.
2. **No implicit unqualified binding from aliased imports.** `import X as x` must never leak `X`'s exports into the caller's short-name namespace, including types, constructors, classes, and values.
3. **Canonical source identity across reexports.** Two aliases of the same source may coexist. Separate physical modules with identically named opaque types remain distinct. Collision/reexport resolution must preserve origin metadata from #1627.
4. **Complete reference rewriting.** References *inside the imported module* continue to resolve against that module's own lexical bindings, including mutually recursive declarations. No implementation may work merely by renaming the imported declaration and leaving its internal references unbound.
5. **Stable type and executable identity.** Qualifiers affect source name lookup only. Do not mix filesystem paths or local alias strings into type identity, frozen `.nbc` artifacts, wire formats, behavior IDs, replay receipts, or runtime schemas.
6. **Consistent frontend and tooling behavior.** Parser, typechecker, effect/capability checks, HIR/MIR lowering, bytecode, native and WASM backends, LSP rename/go-to-definition/completion, formatter, and doc generation must consume a common resolved binding.
7. **Fail closed.** An unknown alias, missing export, nonpublic symbol, conflicting alias binding, or ambiguous legacy import reports a structured error with a useful span. Never silently fall back to an unrelated unqualified name.
8. **Deterministic compilation.** Alias resolution must not depend on import order, filesystem walk order, or hash-map iteration order. Reimports from the same canonical source remain idempotent.
9. **Incremental adoption.** Maintain Nulang 1.x legacy unqualified import behavior for nonconflicting programs. Offer `as` imports as the explicit safe path once end-to-end resolution is implemented and verified.

## Proposed implementation order

1. Provenance completeness for already-existing import forms (this branch).
2. Stable resolver-owned `ModuleBinding` / `ResolvedSymbol` model, with explicit source origin, export visibility, and local name (**direct-import lookup prototype complete, production resolution and reexports outstanding**).
3. AST/parser and formatter retention of `import path as alias` with a fail-closed semantic error (**syntax groundwork completed**); then implement `alias::name` as a distinct unresolved reference form, including unknown-alias tests.
4. Typechecker and effects/capability lookup using resolved symbols. Verify polymorphic constructors and opaque types cannot be confused across modules.
5. HIR/MIR and all backends operate on canonical resolved symbols rather than alias-formatted text. Add cross-backend behavior fixtures.
6. Formatter, LSP, JSON diagnostics, and migration examples, with full CI and conformance.

## Minimum executable regression matrix before feature promotion

- Two colliding function exports from separate modules work when both are aliased, but error when both are legacy unqualified imports.
- Identical short-name opaque types from two modules are not interchangeable.
- Internal calls and mutually recursive functions within imported modules retain correct binding.
- `alias::function` and `alias::Type` resolve only when exported; unknown qualifiers and hidden names fail closed.
- Repeated and transitive imports preserve canonical provenance; different aliases of one module work.
- Nested dependencies do not accidentally inherit caller aliases.
- Module-level `let`, `given`, signal, workflow, and class declarations participate in conflicts.
- Bytecode/native/WASM parity on qualified references and stable artifact/replay metadata.
- LSP navigation and formatter round-trip preserve aliases.
- All legacy nonconflicting import fixtures continue passing unchanged.

**No claim of qualified-import runtime support is made by this document or its companion provenance tests.**
