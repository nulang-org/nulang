# Graphics-oriented C scalar FFI (experimental)

This document records what Nulang's **host-side** FFI can actually do. It
does **not** claim that any graphics backend, GPU driver, or public graphics
package is implemented. See [tracking issue #2642](https://github.com/nulang-org/nulang/issues/2642)
and [draft RFC 0026](https://github.com/nulang-org/nulang/pull/2643).

## Scope

With Cargo feature `ffi`, `src/ffi/marshal.rs` accepts an explicit
`ffi::marshal::Signature` containing these `CType` scalars:

| C ABI type | Nulang host Value | Checks |
| --- | --- | --- |
| `I8`, `I16`, `I32` | `Value::int` | Signed input bounds; checked output |
| `U8`, `U16`, `U32`, `U64` | `Value::int` | Reject negative/out-of-range inputs; checked output |
| `F32` | `Value::float` | Explicit downcast; finite overflow rejected |
| Existing `I64` | `Value::int` | Reject output outside signed 48-bit range |
| Existing `F64` | `Value::float` | Existing behavior |
| Existing `Bool`, `CStr`, `VoidPtr`, `Unit`, `Value` | Existing behavior | Existing ownership/provenance limitations |

Nulang's canonical `Value::int` has a **signed 48-bit payload**. A C
`uint64_t` larger than `2^47-1` cannot round-trip through a Nulang integer
and is deliberately rejected on return. A C `uint32_t` fits fully. Exact
binary interchange of arbitrary `uint64_t` values will require a separate
typed integer/buffer representation, not a silent cast.

`f32` arguments are rounded as required by IEEE-754 conversion from a Nulang
`f64` value; finite `f64` values too large for finite `f32` reject instead
of silently becoming infinity. Explicit `+/-Infinity` remain representable.

## Not yet supported

This change adds **host-side** `CType` variants, not new source-level
`extern` types. `src/bytecode.rs::FfiType` and serialization formats are
unchanged. Source `Int` still maps to C `i64`; source `Float` to C `f64`.
Do not declare a C `uint32_t` function as source `Int` and assume its ABI is
compatible.

When the `ffi` Cargo feature is absent, the fixed-arity native fallback
rejects the new scalar variants. It does not guess unsupported calling
conventions. Its existing `i64` return path now checks the 48-bit result
range as well.

Still needed before shipping any graphics binding:

- C-compatible structs/unions, layout/alignment, pointers/out parameters,
  callback trampolines, and true C-native memory ownership.
- Audited native pointer provenance and cross-actor resource ownership.
- Platform ABI verification, COM/Objective-C bridges, and GL/Vulkan dispatch
  loading.
- Browser JS host adapters for WebGPU and WebGL rather than native `extern`.
- An installable `graphics-webgpu` package and real GPU execution/readback
  tests for every claimed supported target.

Foreign pointers, device handles, and callbacks must never be assumed safe
to serialize, persist, replay, or transfer across actors.

## Verification

From the repository root, with Rust dependencies available:

```bash
cargo test --locked --no-default-features --features ffi --lib ffi::
cargo test --locked --no-default-features --lib ffi::
```

The targeted regression tests are named `graphics_ffi_*`; they cover
signed/unsigned widths, a native `f32` round-trip, rejected narrowing, and
rejected unrepresentable integer returns with/without libffi. These tests
exercise local C ABI calls on the test host. They do **not** validate Vulkan,
WebGPU, Windows COM, Apple Metal, or a browser GPU implementation.
