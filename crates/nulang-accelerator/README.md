# nulang-accelerator

Experimental provider-neutral accelerator primitives for Nulang.

This crate is the boundary between Nulang's actor/effect runtime and concrete
compute backends such as CUDA, ROCm, Metal, Vulkan, IREE, CPU SIMD, and future
NPUs. It intentionally does not add accelerator-specific syntax or opcodes.

## Phase 1

The current implementation provides:

- extensible backend, device, and feature identifiers;
- tensor dtype/shape/layout metadata with overflow-safe size calculations;
- normalized device capabilities;
- hard device requirements and ordered backend preferences;
- deterministic device selection;
- an atomic backend discovery/refresh trait and registry;
- a deterministic CPU reference tensor executor;
- an accelerator-neutral tensor graph IR;
- backend-neutral buffer/session ownership;
- the shared semantics used by the Nulang 2 `Tensor.*` VM effects.

A backend adapter translates native discovery APIs into DeviceCapabilities.
Application/runtime code asks for capabilities rather than naming vendor APIs
directly.

## Design constraints

1. **Accelerators are not a new runtime primitive.** Accelerator execution is
   an effect performed by an actor; device location is placement policy.
2. **No vendor concepts in the frozen language surface.** CUDA/ROCm/Metal
   handles stay behind adapters.
3. **No implicit fallback.** Hard requirements are explicit. CPU fallback must
   be requested by the caller rather than silently changing semantics.
4. **Unknown is not supported.** Backends omit capabilities they cannot prove.
5. **Discovery refresh is atomic.** A malformed backend snapshot cannot erase a
   previously usable device view.

## Next slices

The intended follow-up layers are:

1. IREE/MLIR lowering for heterogeneous GPU/NPU targets;
2. actor placement integration with nulang-capacity and the local device registry;
3. LLM-specific memory planning (weights, KV blocks, prefix caches, spill);
4. batching, graph capture, kernel fusion, and topology-aware multi-device
   scheduling;
5. differential conformance against at least two accelerator backends.

See RFC 0024 for the architecture and compatibility boundaries.
