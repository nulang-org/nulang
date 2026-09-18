# Native runtime embedding

Status: Experimental host integration.

Nulang native/mobile hosts run frozen `.nbc` artifacts through the existing bytecode VM. They do not need to compile application source on-device and they do not introduce a second runtime.

## Build-time flow

1. Compile the application to `.nbc` with the normal Nulang compiler/package pipeline.
2. Bundle the `.nbc` artifact in the native application.
3. Link the Nulang `staticlib`/`cdylib` as appropriate for the host platform.
4. Include `include/nulang_embed.h` from native bridge code.
5. Restricted hosts such as iOS should create the runtime with `nulang_runtime_new_interpreter()`.
6. Load the bundled artifact with `nulang_load_nbc()` and execute its returned public handle with `nulang_run()` or `nulang_call_function()`.

The loader calls the same `CodeModule::from_nbc` decoder used by other Nulang runtime paths, including bytecode-format and language-version validation.

## Interpreter-only constructor

`nulang_runtime_new_interpreter()` preserves the ordinary VM, value layout, FFI surface, `.nbc` format, and module-handle semantics while disabling native JIT tiering for every execution entry point.

This is stronger than relying on a host convention around `nulang_run()`: both top-level execution and exported-function calls use the interpreter-only VM path.

The `mobile-runtime` Cargo profile separately removes native-codegen dependencies from the build graph. Native packagers should use both controls:

```sh
cargo build \
  --release \
  --lib \
  --no-default-features \
  --features mobile-runtime \
  --target <native-target>
```

The root library emits `rlib`, `cdylib`, and `staticlib`, allowing Apple packaging to consume `libnulang.a` and Android/JNI packaging to consume the dynamic library form.

## C ABI ownership

- `NulangRuntime *` is opaque and owned by the caller until `nulang_runtime_free()`.
- Each successful compile/load returns a public module handle. Hosts must pass that handle back to module operations; it is not a direct vector index.
- `.nbc` input bytes are borrowed only for the duration of `nulang_load_nbc()`.
- `nulang_last_error()` returns runtime-owned memory valid until error state changes or the runtime is freed.
- `nulang_value_to_string()` returns runtime-owned cached memory. `nulang_free_string()` can release one returned cached string early.
- `nulang_module_string()` interns a host string into the selected module so it can be passed safely to an exported Nulang function.

## Native callbacks

Host functions may be pre-registered with `nulang_register_native_function()` and imported from Nulang through the existing `__nulang_registered__` sentinel. The C header mirrors the current `CType` ABI, including the opaque raw `Value` variant.

The platform-neutral UI/mobile bootstrap should be layered on this embedding substrate rather than duplicating artifact loading or VM creation in Swift/JNI glue.

## Validation gate

Before treating a native package as distributable, require all of the following on the exact release commit:

```sh
cargo test --lib --no-default-features --features mobile-runtime
bash scripts/check_mobile_runtime_profile.sh
```

Apple/Android packaging jobs should additionally perform real target cross-compilation and inspect the produced library/archive rather than relying on source-level checks alone.
