# RFC 0024: Accelerator Compute Substrate

- **Status:** Draft
- **Tier:** Experimental
- **Created:** 2026-09-21

## Summary

Nulang should make accelerator hardware an execution detail without pretending
that software can replace the throughput or memory bandwidth of GPU/NPU
hardware.

This RFC introduces an accelerator-neutral runtime substrate below the frozen
language surface. The first implementation lives in
crates/nulang-accelerator and covers tensor metadata, normalized device
capabilities, capability-based placement, deterministic device selection, and a
backend discovery boundary.

CUDA, ROCm, Metal, Vulkan, IREE, CPU SIMD, and future NPUs are adapters behind
that boundary rather than language concepts.

## Semantic fit

RFC 0017 defines seven canonical runtime primitives. Accelerator compute does
not add an eighth.

Conceptually:

    accelerator execution
      = actor
      + effect
      + capability/authority
      + placement policy

An actor may own accelerator-resident resources, but actor identity, messaging,
supervision, durability, and time semantics do not change merely because its
compute is placed on a GPU or NPU.

## Goals

1. Let Nulang code request compute capabilities rather than vendor APIs.
2. Keep CUDA/ROCm/Metal-specific handles out of Nulang Core and stable formats.
3. Support deterministic placement across heterogeneous local and remote
   devices.
4. Build a substrate suitable for LLM inference memory planning, batching, and
   distributed model execution.
5. Preserve a CPU reference path for correctness testing and fallback when the
   caller explicitly permits it.
6. Allow Nulang Cloud capacity offers to feed the same placement vocabulary
   without coupling cloud procurement directly to execution internals.

## Non-goals

- Replacing GPU/NPU silicon with ordinary CPUs.
- Adding GPU-specific bytecode opcodes in phase 1.
- Freezing a tensor syntax or tensor ABI before execution semantics are proven.
- Implementing CUDA, ROCm, Metal, and Vulkan code generators independently.
- Making silent device fallbacks that can change latency, precision, or cost.

## Phase 1: neutral device substrate

The first phase introduces:

- BackendId: an extensible normalized backend identifier.
- DeviceId: a runtime-local stable device identifier.
- FeatureId: an open accelerator feature vocabulary.
- DeviceClass: CPU, GPU, NPU, or other.
- DType and TensorSpec: logical tensor metadata independent of allocation.
- DeviceCapabilities: normalized memory, dtype, and feature support.
- DeviceRequirements: hard capability constraints.
- DeviceRequest: hard requirements plus ordered backend preferences.
- AcceleratorBackend: provider-neutral discovery boundary.
- DeviceRegistry: atomic discovery refresh and deterministic selection.

No source-language syntax, .nbc bytecode, value layout, NUL0 packet, or durable
state representation changes in this phase.

## Placement semantics

Requirements are hard constraints. Preferences affect ordering only.

There is intentionally no implicit CPU fallback. If a workload may run on both
GPU and CPU, its request must permit both classes/backends. This avoids a
runtime silently turning a latency-sensitive or precision-sensitive GPU
workload into a materially different execution mode.

Eligible devices are ordered deterministically by:

1. explicit backend preference;
2. available device memory descending;
3. total device memory descending;
4. stable device identifier.

Later placement versions may incorporate topology, measured throughput, power,
cost, locality, and queueing pressure. Those metrics should remain policy, not
language semantics.

## Relationship to nulang-capacity

nulang-capacity currently answers a different question: where should Nulang
acquire compute capacity economically?

nulang-accelerator answers: given visible compute devices, which execution
capabilities are available and where should a workload run?

The two should converge through an adapter rather than by sharing provider
types directly:

    Job / actor placement policy
        |
        +--> nulang-capacity    -> acquire node/instance
        |
        +--> nulang-accelerator -> select local execution device

A later phase may translate accelerator requirements into cloud CapacityOffer
constraints and then re-run local discovery after provisioning.

## Tensor and memory model direction

TensorSpec is intentionally logical. It does not yet expose backend pointers,
streams, command queues, CUDA contexts, or storage ownership.

The next memory layer should distinguish at least:

- device-local memory;
- host-visible/pinned memory;
- unified/shared memory;
- remote or spill storage.

For LLM workloads the runtime should eventually understand logical resource
classes such as immutable model weights, mutable KV-cache blocks, prefix-cache
entries, temporary activations, and durable actor state. These classes have
different eviction and migration policies and should not be collapsed into one
generic byte buffer.

## Compilation direction

Nulang should not begin by writing independent CUDA/ROCm/Metal compilers.

The preferred path is:

    Nulang tensor/graph IR
        -> optimization and fusion
        -> IREE/MLIR-compatible lowering
        -> CUDA / ROCm / Metal / Vulkan / CPU / NPU backends

A CPU reference executor should exist before aggressive accelerator lowering so
graph semantics can be differential-tested.

A native Nulang kernel language is a possible later optimization surface, not a
phase-1 requirement.

## Actor integration direction

An actor may eventually hold an accelerator session and keep model weights or
other hot state resident across messages. That creates a useful unit for LLM
serving:

- actor identity remains stable;
- placement chooses a compatible accelerator host/device;
- mailbox traffic becomes inference work;
- supervision owns device/session failure;
- durable state excludes opaque vendor handles and reconstructs them on
  activation;
- migration drains or reconstructs accelerator resources instead of trying to
  serialize raw pointers.

## Security and authority

Accelerator use must eventually participate in Nulang host authority rather than
being ambient. A likely authority family is Compute::Use(resource), but this RFC
does not freeze token spelling.

Backend adapters must not gain filesystem, network, process, or device access
beyond the authority explicitly granted to the host/runtime embedding them.

## Compatibility

Phase 1 is fully additive and Experimental:

- no frozen language syntax changes;
- no stable artifact changes;
- no wire-format changes;
- no value-tag changes;
- no requirement that existing programs know accelerator types;
- backend identifiers and feature identifiers remain extensible strings.

## Implementation sequence

1. **Implemented in the initial experimental slice:** device/tensor metadata,
   requirements, deterministic selection, backend discovery, atomic registry.
2. Add buffer/session ownership with explicit lifetime and device affinity.
3. Add a small accelerator-neutral graph IR and CPU reference executor.
4. Lower graph IR through IREE/MLIR and benchmark CUDA/ROCm/Metal/Vulkan.
5. Integrate actor placement and host authority.
6. Integrate nulang-capacity for remote accelerator acquisition.
7. Add LLM memory planning, KV-cache paging, continuous batching, and graph
   capture.
8. Add topology-aware multi-device placement and distributed model execution.

## Acceptance criteria before stabilization

The subsystem should not move beyond Experimental until it demonstrates:

- deterministic CPU-vs-accelerator semantic parity on a differential suite;
- backend-independent device and graph descriptions;
- explicit memory ownership with no raw vendor handles in durable state;
- measured inference improvements on at least two materially different
  accelerator backends;
- clean failure/restart behavior through ordinary actor supervision;
- no changes required to frozen Nulang Core or v1 artifact/wire contracts.
