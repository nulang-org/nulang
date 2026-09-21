# RFC 0025: Nulang 2 Development Line

- **Status:** Accepted
- **Tier:** Frozen-version transition / Experimental v2 additions
- **Created:** 2026-09-21
- **Language version:** 2.0.0-dev
- **Supersedes:** no Nulang 1 guarantee; establishes the next major language line

## Summary

Nulang's active compiler moves from language version `1.0.0-frozen` to
`2.0.0-dev`.

This does **not** revoke the Nulang 1 frozen contract. Nulang 1 Core, NBC
format v1, NUL0 v1, and value-layout v1 remain frozen compatibility
boundaries. Instead, Nulang 2 creates a new major language line in which new
syntax, types, effects, and compiler/runtime semantics may remain
Experimental and evolve before the Nulang 2 freeze.

The first Nulang 2 language additions are the accelerator-compute surface:

- `Tensor[T]` as a built-in type constructor;
- `Device` as a built-in opaque compute-device type;
- the `Tensor` effect family;
- the `Compute` effect family;
- compiler-owned signatures for the initial tensor/device operations.

## Motivation

Freezing the entire active language while Nulang is still rapidly exploring
durable execution, heterogeneous compute, distributed state, and AI workloads
creates the wrong optimization pressure. It makes useful experiments look
like permanent language commitments.

At the same time, deleting the Nulang 1 stability contract would undermine the
durability goal that motivated format/version governance in the first place.

A major development line separates those concerns:

1. Nulang 1 remains a permanent compatibility baseline.
2. Nulang 2 may add and revise explicitly Experimental surfaces.
3. Promotion into Stable/Frozen remains deliberate rather than accidental.

## Version semantics

The compiler/runtime constants become:

- language version: `2` / `2.0.0-dev`;
- NBC format version: remains `1`;
- NUL0 wire version: remains `1`;
- value-layout version: remains `1`.

No binary layout is changed merely by entering the Nulang 2 language line.

Every newly emitted `.nbc` artifact records language version 2. A Nulang 1
runtime therefore rejects it as a future language artifact instead of
silently executing semantics it does not know.

The Nulang 2 runtime continues accepting language-version-1 artifacts whose
NBC format version it supports. This provides the migration direction:

    Nulang 1 artifact -> Nulang 2 runtime     allowed
    Nulang 2 artifact -> Nulang 1 runtime     rejected
    NBC v1 bytes       -> still NBC format v1

A future binary-layout change must independently bump the corresponding format
version and provide a migration under RFC 0001. A language-major bump is not
permission to reinterpret frozen bytes.

## Source compatibility

Nulang 1 Core remains valid Nulang source under Nulang 2. The guarantee from
RFC 0002 is inherited rather than discarded.

Nulang 2 additions are Experimental by default until an RFC explicitly
promotes them. Experimental v2 syntax/types/effects may change without a
multi-major deprecation cycle while the language version carries the
`-dev` designation.

This rule applies only to newly introduced or explicitly v2-Experimental
surfaces. It does not retroactively demote Nulang 1 Frozen or Stable
contracts.

## Accelerator language surface

### Types

```nulang
Tensor[Float]
Device
```

`Tensor[T]` carries an element type through HM type checking. Shape is
runtime metadata in the first implementation; shape types are deliberately
not frozen yet.

`Device` is opaque at source level. Its current runtime representation is an
implementation detail and may change during 2.0 development.

### Tensor effects

Initial compiler-owned signatures:

```text
Tensor.from_array([Float], Int, Int) -> Tensor[Float]
Tensor.zeros(Int, Int)               -> Tensor[Float]
Tensor.shape(Tensor[T])              -> [Int]
Tensor.to_array(Tensor[Float])       -> [Float]
Tensor.add(Tensor[Float], Tensor[Float])       -> Tensor[Float]
Tensor.matmul(Tensor[Float], Tensor[Float])    -> Tensor[Float]
Tensor.relu(Tensor[Float])                    -> Tensor[Float]
```

The current runtime executes these through the deterministic CPU reference
backend. Accelerator backends must preserve the same observable tensor
semantics subject to documented floating-point tolerances.

### Compute effects

```text
Compute.default_device() -> Device
Compute.device(String)    -> Device
Compute.device_name(Device) -> String
```

Actor-backed device acquisition is authority-gated by
`Compute::Use(resource)`. Top-level/standalone execution retains the
existing ambient host contract.

## Runtime representation

This RFC does not allocate a new NaN-boxed value tag or bytecode opcode.

The initial VM representation uses an opaque runtime-owned heap object for
tensor storage and an opaque device handle. This is intentional: the language
surface can be tested before committing the frozen value layout to accelerator
implementation details.

Native CUDA/ROCm/Metal/IREE handles must never enter durable serialized state.
They are reconstructed from placement/session metadata after activation or
restart.

## Compiler architecture

The intended lowering path is:

```text
Nulang source
  -> typed Tensor/Compute effects
  -> ordinary HIR/MIR effect lowering
  -> CPU reference semantics today
  -> accelerator graph IR
  -> IREE/MLIR lowering
  -> CUDA / ROCm / Metal / Vulkan / CPU / NPU
```

A future optimization pass may recognize groups of Tensor effects and lower
them into the accelerator graph IR without changing source semantics.

## Compatibility and migration

Because the first accelerator surface is additive, no Nulang 1 source rewrite
is needed.

The required migration behavior for the major line is:

- keep Nulang 1 Core parse/type/runtime semantics;
- accept NBC language version 1 in a version-2 runtime when the format version
  is supported;
- reject language version 2 in a version-1 runtime;
- never reuse frozen opcodes, value tags, or wire packet meanings;
- introduce explicit `nulang migrate` rewrites before any future Nulang 2
  change that would invalidate non-Core source.

## Stabilization criteria

The `2.0.0-dev` suffix must not be removed until:

1. Nulang 1 compatibility tests pass under the v2 runtime.
2. New v2 syntax has a conformance suite.
3. Accelerator semantics pass differential CPU-vs-device tests.
4. At least two materially different accelerator backends execute the same
   graph semantics.
5. Durable state contains no native accelerator handles.
6. The migration story for every breaking v2 development change is recorded.
7. The steward accepts a Nulang 2 freeze RFC.

## Resolution

Accepted by the language steward on 2026-09-21. The active compiler enters the
Nulang 2 development line while preserving Nulang 1 as a frozen compatibility
baseline.
