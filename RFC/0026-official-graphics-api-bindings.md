# RFC 0026: Official Optional Graphics API Bindings

- **Status:** Draft
- **Tier:** Experimental (packages); any future Stable language or ABI change requires its own accepted RFC
- **Author:** Nulang contributors
- **Created:** 2026-11-12
- **Related issue:** [#2642](https://github.com/nulang-org/nulang/issues/2642)
- **Related draft:** [#1818](https://github.com/nulang-org/nulang/pull/1818) (WebGL2 interoperability pilot, not a completed binding)

## Summary

Nulang should provide officially maintained, **optional**, versioned bindings for
OpenGL/OpenGL ES, Vulkan, Direct3D 11, Direct3D 12, Metal, WebGPU/wgpu-native,
and WebGL 1/2. These are low-level graphics/GPU-compute capabilities, not a
custom Nulang renderer. The initial implementation should deliver one genuinely
working portable path (WebGPU) before adding seven declaration-only packages.

The bindings live outside Nulang Core and introduce **no required graphics
dependencies** for the compiler, VM, or ordinary applications. Keep
[DR-001](../docs/DECISIONS.md) (DOM/native controls/WebView for general UI)
unchanged.

## Motivation and current constraints

Odin demonstrates the developer-experience benefit of first-party bindings,
but its implementation cannot be imported wholesale into Nulang. Nulang's
current FFI is not yet a complete C/COM/Objective-C graphics ABI:

- `src/ffi/marshal.rs` exposes `I64`, `F64`, `Bool`, `CStr`,
  `VoidPtr`, `Unit`, and opaque `Value`, but not general by-value
  aggregates, precise fixed-width scalar types, callbacks, or validated
  ownership/lifetime rules needed by these APIs.
- Native dynamic loading via `extern` uses the optional `ffi` feature;
  the bytecode interpreter is the semantic reference. Native AOT and
  browser WASM do not inherit verified native-FFI parity.
- Existing FFI pointer-provenance/security work is incomplete and must not be
  bypassed by exposing raw GPU pointers as ordinary managed values.
- A browser `WebGL2` example does not prove a public WebGL/WebGPU library,
  driver execution, or platform portability.

## Design decision: official registry packages, not a new import namespace

Use Nulang's existing package distribution and versioning rather than
adding an Odin-style `vendor:` compiler import namespace.

Proposed public package names:

| Package | API / scope | Runtime environment | First delivery |
| --- | --- | --- | --- |
| `graphics-webgpu` | portable WebGPU API; native wgpu implementation adapter | native and browser (distinct host adapters) | P0 |
| `graphics-opengl` | OpenGL / OpenGL ES, context and symbol loading | native desktop/mobile where available | P1 |
| `graphics-vulkan` | Vulkan loader, dispatch, extensions | supported native Vulkan targets | P1 |
| `graphics-metal` | Metal with audited Objective-C bridge | Apple native targets | P2 |
| `graphics-d3d12` | D3D12, DXGI, shader toolchain integration | Windows | P2 |
| `graphics-d3d11` | D3D11, DXGI, shader toolchain integration | Windows | P3 |
| `graphics-webgl` | WebGL 1 and 2, with feature-negotiated interfaces | browser WASM/JS host | P3 |

Names are proposals until the package registry validates them. Package names,
API revision, host implementation revision, target matrix, and external
runtime requirements must be published in each package's metadata/README.
User programs must explicitly opt in through the package manager.

Every package has two explicitly distinguished layers:

1. **Raw binding:** versioned ABI-exact declarations, generated where practical,
   with upstream-compatible naming and no fabricated safety guarantees.
2. **Convenience layer:** small idiomatic helpers for feature detection, error
   conversion, resource release, and context/device ownership. No general
   cross-API renderer, shader language, or UI toolkit is part of this RFC.

A common package API may exist later only for demonstrably shared semantics;
do not force Vulkan, OpenGL, and WebGPU into a lossy common abstraction.

## Host and ABI contract

Implement graphics ABI support as a separate foundational milestone. Do **not**
mark a backend supported merely because its declarations parse.

Required verified capabilities:

- Correct C scalar widths and signedness (`i8/u8/i16/u16/i32/u32/i64/u64`,
  `f32/f64`), including endian and target-specific size/alignment rules.
- C-compatible structs/unions/enums, pointer-to-pointer and out parameters,
  nullable pointers, and arrays/buffers with explicit length and provenance.
- Function pointers and callback trampolines with safe registration,
  cancellation, destruction and foreign-thread entry.
- Correct per-target calling conventions, including Windows COM vtables,
  HRESULT/GUID representation, and Apple Objective-C message dispatch,
  retention/autorelease and required thread restrictions.
- Dynamic function resolution for GL/Vulkan and explicit platform load
  failures; do not guess a missing extension's availability.
- A native-pointer representation that cannot be mistaken for an actor
  heap pointer, forged through untrusted values, serialized through actor
  messages, or dereferenced after destruction.
- Backend-specific `extern` validation: unsupported target/ABI calls must
  fail closed, not silently route to an incompatible backend.

If these primitives would change Stable language semantics or the Frozen
value-layout ABI, split the change into a dedicated governed RFC and
version/migration plan. This RFC does **not** itself authorize those changes.

A narrow, audited C host adapter is permissible for the first experimental
WebGPU pilot while this general ABI work is underway, provided it uses
opaque validated handles, does not expose raw pointers to untrusted Nulang
values, and does not claim to be a complete raw WebGPU binding.

## Platform-specific behavior

**WebGPU / wgpu-native.** Treat the browser WebGPU host and native wgpu
implementation as separate backends of a versioned API contract. Pin the
C-header/implementation revision for native calls. Adapter/device
initialization and asynchronous request/error callbacks must have documented
lifetime and event-loop behavior. Browser support requires explicit JS host
adapters; plain WASM `extern` cannot call a browser GPU API by itself.
Unsupported features produce explicit errors rather than assuming parity.

**OpenGL.** Context creation is a separate windowing/EGL/GLX/WGL concern.
Resolve extension entry points through the active context's resolver.
Publish supported GL/GLES profiles and loader behavior. Do not assume every
GL function is an ordinary dynamically exported shared-library symbol.

**Vulkan.** Generate from a pinned Khronos Vulkan registry/header version.
Preserve extension gates, platform-specific definitions, C structure layout,
and instance/device dispatch tables. Avoid checked-in handwritten copies of
thousands of declarations.

**Direct3D 11 / 12.** Use a Windows-only COM-compatible bridge, including
DXGI and required shader compiler interfaces. Preserve interface versioning,
reference-count semantics, and return code checks. Do not bundle proprietary
SDK binaries in the compiler or base packages.

**Metal.** Use an audited native Objective-C bridge, with Apple SDK and
runtime availability checks. Correctly handle retained objects, autorelease
pools, selectors and calls whose ABI differs by architecture.

**WebGL 1 / 2.** Use browser JS host adapters with feature negotiation,
typed-array transfer rules, asynchronous frame scheduling, context loss and
restoration. WebGL 1 and WebGL 2 feature sets must remain distinct; Canvas
fallback is useful but does not constitute successful GPU validation.

## Safety, effects, and actors

A GPU context/device/queue/encoder is a resource bound to a specific runtime
owner, with additional backend-specific threading constraints. Never assume
that GPU objects are freely transferable or `Send` across actor shards.
Transfer validated data or explicit authority-checked handles instead.

GPU commands, synchronization, readbacks and driver callbacks are external,
potentially nondeterministic effects. They must not run implicitly during
durable workflow replay. Require explicit host capability authorization,
resource quotas, release operations, and bounded submission/readback queues.
Document device loss and teardown on abnormal actor termination.

Do not allow raw native GPU pointers to cross distributed actor messages,
untrusted host calls, or persisted snapshots.

## Generator and dependency policy

- Pin upstream schemas/headers and tool versions; record URL, revision,
  checksum, licenses and any local transformation.
- Prefer published Khronos OpenGL/Vulkan registries for generation.
- For WebGPU, pin the selected native header/implementation and separately
  track browser API compatibility; do not imply that wgpu-native and the
  browser are ABI-identical.
- Use checked-in, reproducibly generated source with deterministic diff
  validation (regenerate then require an empty diff).
- No unreviewed wholesale copies of Odin binding sources; inspect licenses
  and attribute material correctly.
- Keep all packages opt-in, with no new default Cargo features or graphics SDK
  requirements in a minimal Nulang installation.

## Implementation sequence and acceptance evidence

### P0: Prove an executable WebGPU path

1. Establish native ABI/layout test fixtures (including Windows and ARM
   concerns) and foreign-pointer/callback safety tests.
2. Reconcile outstanding FFI hardening before exposing new unsafe bindings,
   including the relevant draft work in #2459 and #2622.
3. Add `graphics-webgpu` as an experimental package, version-pinned, with a
   minimal adapter/device creation, buffer allocation/write/readback, small
   compute or render pass, error handling, device loss and teardown example.
4. Add an explicit browser WebGPU host implementation with a separate browser
   smoke; never report a compiler-only test as GPU execution.
5. Run native smoke on actual GPU hardware **or a documented GPU software
   implementation** and browser smoke on a browser supporting the API.
   State exact OS/architecture/driver/browser and evidence for each run.

**P0 completion:** a fresh consumer can install the package, execute the
example and tear down resources on one verified native target and one
verified browser target, with no changes to a minimal Nulang dependency
profile.

### P1: OpenGL/GLES and Vulkan

- Generate versioned declarations from pinned upstream registries.
- Verify struct ABI and dispatch/loaders, not just source compilation.
- Run one graphical smoke per supported target/profile and a headless compute
  or buffer verification for Vulkan where supported.
- Ensure unsupported extension/feature requests reject explicitly.

### P2: Metal and Direct3D 12

- Add per-OS audited bridge and lifetime tests.
- Validate real host builds and at least one hardware/software smoke on each
  claimed platform.

### P3: Direct3D 11 and WebGL 1/2

- Deliver compatibility bindings with full explicit feature detection.
- For WebGL, reconcile and qualify the existing draft #1818 rather than
  assuming its current example proves API support.

## Release/quality gates

A package can be tagged **experimental** when its interface and local tests
exist, but **supported on a target** only after all of:

1. A runnable example executed on the exact target/backend, with a recorded
   output or readback check (not just absence of exceptions).
2. ABI/layout/callback/lifetime tests for that target.
3. Missing-library, unsupported-extension, null-return, device/context-loss,
   cleanup and memory-budget failure tests where applicable.
4. Reproducible generated bindings checked against pinned upstream inputs.
5. Package-manager publish/install/consumer round-trip and documented
   dependency/license inventory.
6. CI evidence for applicable Linux x86_64/aarch64, macOS aarch64, Windows
   x86_64 and browser lanes. Unsupported lanes are explicitly labeled;
   no target is claimed supported merely because cross-compilation passes.
7. Exact-head checks complete; partial or missing GPU test evidence remains
   visible rather than being inferred from other tests.

## Tier classification and backwards compatibility

This RFC specifies optional **Experimental** packages only. It does not
modify Nulang Core, the existing `extern` syntax, any Frozen bytecode/wire
artifact, or general application rendering. Existing programs and minimal
builds remain unchanged.

A later compiler/ABI RFC is necessary if implementation requires changes to
Stable language semantics or Frozen value layout; migration and versioning
requirements follow `GOVERNANCE.md`.

## Alternatives considered

- **Bundle seven graphics APIs into the compiler/stdlib:** rejected because
  it adds cross-platform SDK/version churn to every Nulang installation.
- **Add an Odin-style `vendor:` syntax now:** rejected; the existing registry
  can distribute first-party packages without a new language feature.
- **Expose only Vulkan or raw D3D12:** rejected as the first step because it
  delays a useful portable GPU path and increases early ABI surface.
- **Provide a custom universal graphics renderer:** rejected under DR-001;
  a separate high-level library may be proposed if real demand appears.
- **Check in seven declaration-only stubs immediately:** rejected; this
  inflates support claims without testing ABI, lifetime or platform behavior.

## Open questions

- Should the portable public package use `graphics-webgpu` alone, or publish
  a `graphics-wgpu` compatibility alias once the native backend matures?
- Which exact native C WebGPU header/implementation and browser API revision
  should be the first baseline, and how should callback-version transitions
  be handled?
- Which hardware-backed CI runners can regularly validate Windows/Apple/GPU
  paths without making regular compiler CI depend on scarce machines?

## Resolution

Pending discussion and maintainer acceptance. No API is claimed implemented
or supported by this RFC alone.
