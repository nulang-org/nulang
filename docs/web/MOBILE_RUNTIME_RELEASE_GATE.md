# Mobile runtime release gate

The `mobile-runtime` Cargo profile is a distribution boundary, not merely a
runtime setting.

Every change must preserve all of the following:

1. `cargo check --lib --no-default-features --features mobile-runtime` passes.
2. Library tests pass under the same feature set.
3. The normal/build dependency tree contains no Cranelift crates, Wasmtime, or
   `libloading`.
4. Cranelift and `target-lexicon` root dependencies remain owned by the
   `native-codegen` feature.
5. `mobile-runtime` does not enable `native-codegen` or dynamic FFI loading.
6. Default builds retain `native-codegen` until that public default changes
   intentionally.

The canonical local/CI command is:

```sh
bash scripts/check_mobile_runtime_profile.sh
```

## Why this is required

Disabling JIT execution at runtime is insufficient for an Apple binary: code
for runtime executable-code generation can still be linked into the artifact.
The mobile profile therefore removes native-codegen dependencies from the Cargo
graph and also compiles/tests the resulting interpreter-only source graph.

Do not publish an Apple static library or XCFramework unless this gate is green
on the exact commit being packaged.
