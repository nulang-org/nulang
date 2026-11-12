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

## Diagnostic incremental correctness oracle

The `package::incremental_oracle` module provides a **non-production audit**
for proposed incremental builds. It is intentionally independent of the CLI,
package resolution, cache, VM, JIT, AOT, and WASM backends.

The caller supplies five things:

1. Exact before and after `ModuleSnapshot` values, with compiler-owned
   resolved imports and a complete compiler-context digest.
2. `BuildObservation` from a **clean compilation of the before tree**.
3. `BuildObservation` from an **independent clean compilation of the after tree**.
4. Optionally, a `BuildObservation` from the proposed incremental build.
5. Canonical, deterministic compiler-output bytes at each stage: diagnostics,
   typed/effect/authority facts, MIR, and final artifact. The oracle stores
   BLAKE3 digests of those bytes, not their unverified source-level guesses.

The oracle verifies that all observed module names and context digests match the
claimed snapshots. A missing, extra, or structurally incomplete output is an
error. The oracle rejects an invalidation frontier if any allegedly reusable
module's clean-before and clean-after observations differ at any stage. When a
candidate is supplied, it additionally compares **every** candidate module to
its independently compiled clean-after result, including dirty modules.

Possible over-invalidation is reported separately as a performance lead, not
as permission to skip compilation. A full-rebuild decision stays a full
rebuild; the oracle cannot bypass it. A consistent report is **not proof of a
complete compiler input closure** and never authorizes cache reuse or runtime
deployment.

The test corpus exercises missed semantic changes, stale artifacts, incomplete
and wrong-context observations, over-invalidation reporting, and a real
lexer → parser → typechecker → HIR → MIR → bytecode compilation pair. It does not
yet exercise multi-module compilation through a complete compiler-owned
resolved dependency graph, because that collection/wiring has not landed.

```sh
cargo test --no-default-features --test incremental_oracle
cargo test --no-default-features --lib package::module_invalidation::tests
```

Before production incremental reuse, integrate compiler-owned input collection,
capture canonical output bytes for all compilation paths, expand the edit corpus
to imports, effects, authority, actor protocols, durable schemas and backend
configurations, and run the oracle as a CI parity gate. Only then measure
whether caching justifies its memory, disk and maintenance costs.

## Standalone clean bytecode observations (stacked follow-up)

`package::clean_observation::observe_unlinked_bytecode_sources` now provides
compiler-produced stage observations for **standalone pure-function modules**.
It takes a complete fixture of named source byte slices; each source goes
through Nulang's real lexer, parser, effect/type checker, function capability
analysis, HIR/MIR lowering, canonical MIR serialization, and bytecode
generation. The input snapshot and the observation share a diagnostic-only
compiler recipe and source-byte fingerprints. Modules are compiled from
scratch and cannot import one another.

The collector **rejects** missing/duplicate module names, non-UTF-8 bytes,
parse/type/effect/capability errors, imports, actors, workflows and every
non-function declaration. It deliberately has no knowledge of the user's
filesystem, imported modules, package graph or resource grants; these require
the production resolver's compiler-owned dependency closure.

Integration tests compare real before/after compiler observations for
private-body edits, exported-signature changes, unchanged independent modules,
and stale candidate bytecode. They verify the oracle's rejection and
consistency paths, rather than relying on fabricated compiler fingerprints.

```sh
cargo test --no-default-features --test incremental_clean_observation
```

**Limit:** This remains a diagnostic test fixture, not the general Nulang CLI
frontend or an incremental compiler. In particular, successful standalone
observations neither authorize skipping normal frontend checks nor establish
equivalence for imports, effects, authority, actor protocols, durable schema
changes, backend flags, or failed compilations. The diagnostic context digest
is not a production artifact/cache key. These categories must be covered by a
compiler-owned multi-module collector before cache reuse can be proposed.

### Stable temporary naming for repeatable artifacts

The clean-observation regression detected a genuine nondeterministic output:
`hir_lower::fresh_temp_name` previously used a process-global atomic counter.
Recompiling the exact same function generated different compiler-only names
(`__tmp0` versus `__tmp1`) in `debug_functions.locals`, changing the
serialized NBC bytes while canonical typed/MIR semantics stayed identical.

HIR lowering now scopes generated temporary names to each `lower_module`
invocation, including re-entrant nested lowering, with a separate fallback
counter for standalone helper calls outside a module. The wire encoding and
debug metadata schema are unchanged. The bytecode-determinism regression test
checks exact debugger metadata and serialized NBC byte equality across
repeated clean compiles; the oracle corpus also checks independent observations.
