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
