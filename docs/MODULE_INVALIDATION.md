# Module invalidation prototype

Status: **diagnostic-only**. No production compilation or cache behavior changes.

The `package::module_invalidation` module computes the set of modules
needing reanalysis when an exact compiler-input snapshot changes. The
prototype is intentionally decoupled from the CLI and VM/JIT/AOT/WASM
backends until compilation parity is demonstrated.

## Contract

Each snapshot is supplied by compiler-owned code and contains:
- Canonical resolved module names and their **complete** imports.
- Digests of the exact source bytes (not semantic equivalence).
- A fingerprint of every non-module compilation input, including
  toolchain, target, backend, flags, ABI, standard-library versions,
  package/dependency manifests, features, and compiler options.

The snapshot type does not currently prove the caller supplied complete input
closure. **Nothing in the output authorizes cache reuse or deployment.**

`invalidated_modules(before, after)` returns:
- `Unchanged` if the supplied snapshots agree.
- `Dirty { modules }` for changed modules and all reachable reverse
  dependents (deterministic lexical ordering).
- `FullRebuild` when compiler context changes, a module is removed, or
  either resolved module graph is incomplete or cyclic.

The graph algorithm uses historical and current dependency edges and does not
parse imports using regexes or duplicate Nulang language semantics. Unknown
imports trigger full rebuild rather than assuming an unaffected cache.

## Acceptance gates before incremental code reuse

1. Wire input collection only through the canonical parser/module resolver.
2. Add compiler-owned inputs for all transitive dependencies, imported
   standard-library modules, native/FFI assets, effect and capability
   configuration, target options, and semantic contracts.
3. Compare fresh and cached diagnostics, type/effect results, MIR, and
   executable behavior over an edit corpus.
4. Compare exact artifact digests for clean and incremental builds with the
   same compilation recipe and enforce fail-closed parity.
5. Benchmark cold builds and warm single-module edits, and retain the change
   only if measurable improvement justifies the memory/maintenance cost.

## Focused validation

```sh
cargo test --lib package::module_invalidation::tests
cargo fmt --all --check
```

Nulang Cloud remains unchanged; this module cannot authorize shared cache
entries, admission, placement, or metering.

## Pascal-inspired public interface impact diagnostics

`ModuleInterfaces` records a BLAKE3 digest of canonical, compiler-emitted
public interface bytes for **every** resolved module in a snapshot. These
bytes must cover exported types, effect rows, reference capabilities,
actor behavior/protocol identity, and schema/ABI contracts. There is no
compiler-owned interface emitter wired into this diagnostic path yet, so
callers cannot treat supplied bytes as verified semantic fingerprints.

`analyze_interface_invalidation(before, after, before_interfaces,
after_interfaces)` separates locally changed modules from their potentially
affected reverse dependents:

- A private source-body edit with unchanged imports and public interface
  remains a local change in the **diagnostic report only**.
- A changed exported interface propagates through reverse dependencies.
- A changed module import set propagates conservatively, even when the
  supplied interface bytes match.
- Missing/extra observations, compiler-context changes, invalid graphs,
  and module removal require a full rebuild.

The preexisting `invalidated_modules` function is unchanged and still
conservatively invalidates all importers of a changed source module.
Neither interface impact diagnostics nor the incremental correctness
oracle authorize skipping semantic checks, reusing cached objects,
or changing CI, cloud placement, or deployment behavior.

To make separate compilation production-safe, implement compiler-owned
canonical interface serialization, account for implicit dependencies,
compare clean/incremental diagnostics, HIR/MIR and emitted artifacts,
and benchmark repeated edits before enabling reuse.

### Compiler-backed function interface prototype

The diagnostic serializer
canonical_checked_function_interface_bytes(ast, checked_types, checked_effects)
uses the real checked module AST, TypeChecker.inferred_decl_types, and
EffectChecker.function_row results. It canonicalizes complete inferred function
types (including effect rows and reference capabilities) through the same
alpha-normalizing TypeSchemaEncoder used by typed semantic identities.
Symbol names are sorted, and serialization has a versioned domain separator.

The current module resolver does not fully enforce source visibility boundaries,
so this prototype includes ALL top-level functions, not just public declarations.
It refuses modules containing imports, nested modules, actors, durable schemas,
type declarations, classes, FFI, or other unsupported declaration kinds.
Callers must run complete type/effect checks first; the helper cannot prove
their provenance or authorize reusing an artifact. Changes to private bodies
may still alter inlined/linked executable artifacts, even when an exported
signature remains the same.

The focused regression suite parses and type-checks real Nulang programs
and verifies private-body stability, inferred return changes, effect changes,
callee-effect propagation, function-order normalization, and unsupported
declaration rejection.

The existing conservative invalidation algorithm remains the only planner
used by the separate diagnostic incremental correctness oracle. Until that
oracle is integrated and a complete compiler-owned dependency closure is
available, interface-impact reports are **advisory only**.

Function signatures also encode explicit function/parameter capability
annotations, parameter names, generic-parameter names, and public flags.
Defaults, implicit using parameters, typeclass constraints, preconditions,
postconditions, and tool/placement annotations are currently rejected,
rather than being silently omitted. The function-only serializer is
not called by the production compiler CLI.

A regression test compiles two source programs whose interfaces are equal
but whose emitted bytecode differs. This demonstrates why public-interface
equality cannot establish executable-artifact reuse, and why the older
source-based conservative invalidation frontier still governs any candidate
full/incremental build comparison.

A separate, temporary smoke worktree may exercise this branch against the
incremental correctness oracle under development, but that oracle is not
merged into this branch and no cache is activated.
