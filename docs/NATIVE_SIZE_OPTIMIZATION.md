# Native code-size optimization (experimental)

The `native` backend can select Cranelift's speed-and-size optimization policy:

```sh
nulang --backend native --native-opt small --eval '1 + 2 * 3'
```

By default, or with `--native-opt speed`, the backend retains its existing
`speed` policy. Invalid modes are rejected; passing `--native-opt` with
another execution backend fails instead of silently ignoring the setting.

## Important limitations

- `--native-opt small` changes **generated native machine-code optimization**
  inside the current in-memory Cranelift JIT module. It does **not** make the
  Rust `nulang` compiler binary smaller.
- This is **not** a native executable emitter. `src/aot/mod.rs` uses
  `cranelift_jit::JITModule`; `--out` currently writes an assembly *placeholder*
  rather than a linkable object, assembly output, or an executable.
- Cranelift's `speed_and_size` can make code smaller, but no guarantee of a
  particular byte reduction is made. Measure the same workload and toolchain
  under both policies before asserting a win.
- Existing native-backend language-support restrictions still apply.

## Verification

Focused unit and integration checks:

```sh
cargo test --locked --features native-codegen optimization_profile_tests
cargo test --locked --features native-codegen --test native_size_optimization
```

The whole suite, including default and minimal Cargo feature configurations,
remains a separate promotion requirement.

## Next architectural step

For actual small standalone Nulang binaries, split AOT code emission from
the JIT-specific module interface; use `cranelift_object::ObjectModule` to
emit linkable native object files; supply a small C-compatible runtime shim
for genuinely referenced helpers; and drive the platform linker with
reachability-based dead-code elimination. Prove pure arithmetic and recursion
parity before adding allocation, effects, or actors.
