# Perform dispatch-cache acceptance criteria

Issue: #369

Generic `Perform` bytecode carries a stable qualified operation name such as
`Float.sqrt` in the module constant pool. The optimization target is runtime
name-resolution overhead, not the serialized bytecode format.

`PerformDirect` is a separate opcode: it already identifies a statically
resolved handler table and binding by index. Stage 1 therefore optimizes
`step_perform`, not `step_perform_direct`.

## Baseline measurements

`benches/vm_bench.rs` contains dedicated hot-loop cases for:

- `Float.sqrt`
- `Int.to_float`
- `Array.length`
- `String.length`
The generic `Perform` group covers the four builtin operations above. A
separate `vm/perform_direct/custom_handler` control benchmark exercises a
statically resolved user handler; source-level known handlers lower to
`PerformDirect`, so that result must not be mixed into the Stage 1 generic
`Perform` speedup calculation.

Run the generic cache baselines with:

```sh
cargo bench --bench vm_bench -- 'vm/perform/'
```

The builtin benchmark sources intentionally execute the same generic
`perform Effect.op(...)` site repeatedly so parsing, allocation, and builtin
fallback remain visible in the result. Handler semantics are protected
separately by the conformance case and the `PerformDirect` control benchmark.

## Semantic gate

`conformance/behavior/perform_direct_01_handler_precedence.nula` installs an
explicit handler around the builtin `Float.sqrt` operation and verifies that
the handler result wins over the builtin fallback. It also covers a custom
handled effect.

Any cache implementation must preserve that ordering:

1. resolve the stable `Perform` identity without per-call owned-string construction;
2. search applicable explicit handlers with the current semantics;
3. only if unhandled, dispatch through runtime callbacks / builtin fallback;
4. unknown operations retain the generic path.

## Stage 1 target

Cache parsed qualified names at module-load time and remove the current
execution-path chain of cloning the qualified constant and allocating owned
effect/op strings.

A compact representation can keep one owned shared qualified name plus the dot
offset, for example conceptually:

```rust
struct CachedPerformName {
    qualified: Arc<str>,
    dot: Option<usize>,
}
```

Cloning the descriptor then requires only a shared-string refcount operation;
effect and operation are borrowed slices of that local descriptor. This avoids
retaining a borrow into `VM.modules` while `step_perform` mutates frames,
handlers, or callbacks.

Build the cache only for constant indices referenced by generic `Perform`
instructions. Runtime subsystems can append new string constants to a loaded
module, so the cache must not assume the module constant-pool length remains
fixed. Existing `Perform` indices and their original constant contents are
the stable identity.

No opcode values, `.nbc` fields, effect signatures, or handler behavior
should change.

## Stage 2 target

After Stage 1 is benchmarked, optionally classify recognized fallback
operations into compact internal `BuiltinId`s. Keep this cache VM-local and
reconstruct it deterministically whenever a module is loaded.
