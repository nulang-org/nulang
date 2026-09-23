---
title: Performance
description: Nulang's execution architecture — semantic-reference bytecode, Cranelift JIT tiering, portable WASM, secondary native AOT, SIMD, memory layout, and benchmark methodology.
---
import JitDemo from '../../../components/animations/JitDemo.astro';

## Backend contract

Nulang deliberately separates **semantic correctness** from execution optimizations:

- **Bytecode VM** — the semantic reference implementation. Full-language behavior is judged against this path.
- **Cranelift JIT** — an optimization layer for hot bytecode regions. A JIT speedup is valid only when it preserves bytecode semantics.
- **WASM** — the canonical portable/cloud execution target. The backend is still experimental and does not yet implement every runtime surface.
- **Native AOT** — a secondary backend for supported programs and differential/conformance testing. It must not be treated as full-language parity until the relevant semantics are proven.

This ordering matters when reading performance results: faster execution on a restricted backend is not evidence that the same speedup applies to arbitrary Nulang programs.

## JIT Tiering

<JitDemo />

Nulang's bytecode VM uses hot-counter tiering to compile frequently-executed code paths:

- **Threshold**: After 1,000 invocations at a given PC, the VM triggers Cranelift compilation.
- **Region compilation**: The compiler scans up to 500 instructions, stopping at unsupported opcodes or `Ret`.
- **Typed compilation**: When register types are statically provable (via `TypeMetadata`), the typed compiler strips NaN-tag guards, emitting unboxed integer and float operations directly.
- **SIMD auto-vectorization**: Element-wise loops on arrays are detected by the SIMD analyzer and compiled to `I64x2`, `F64x2`, `I32x4`, or `F32x4` vector instructions with scalar prefix/epilogue.

Warm-up behavior: the first 1,000 iterations of a hot loop run in the interpreter. Once the threshold is crossed, the compiled region replaces interpretation for subsequent iterations.

## Secondary native AOT

The `--backend native` path lowers supported MIR through Cranelift to native code:

- **Pipeline**: Source → AST → HIR → MIR → Cranelift CLIF → native object code.
- **Typed lowering**: Compile-time type metadata can remove tagged-value checks for supported integer and float operations.
- **No interpreter dispatch for compiled functions**: supported code executes as native machine code rather than through the bytecode dispatch loop.

:::caution[Semantic scope]
Native AOT is **not** the semantic reference implementation. Some effects, continuation/resume forms, actors, FFI, and other runtime-integrated constructs remain restricted or backend-specific. Unsupported constructs must fail deterministically rather than silently changing behavior. Use bytecode when you need the broadest language/runtime coverage.
:::

## WASM Backend

The WASM backend (`--backend wasm`, requires `--features wasm-backend`) compiles MIR to WebAssembly:

- **Compiler**: `wasm-encoder` emits `.wasm` modules with i64-tagged values to avoid WASM NaN canonicalization.
- **Runtime**: Wasmtime host runtime with 4 GiB guard pages, Cranelift speed optimizations (inlining enabled), and SIMD support.
- **AOT compilation**: `wasmtime compile` produces `.cwasm` files for instant startup — no JIT warm-up on the client side.
- **SIMD lowering**: MIR array operations lower to WASM SIMD instructions via raw byte emission.

:::caution[Semantic scope]
The plain WASM backend is still a restricted profile for runtime-integrated effects and actor behavior. The experimental `wasmfx-backend` adds stack-switching lowering for supported suspending effects, but user-defined handler/resume semantics and host-side continuation integration are not yet complete. Treat both WASM paths as experimental until conformance coverage proves parity for the workload you need.
:::

## Register VM

The bytecode VM is designed for compact code and fast dispatch:

- **32-bit instructions**: `{opcode: u8, op1: u8, op2: u8, op3: u8}` — fixed-width, cache-friendly.
- **256 registers per frame**: Flat `Vec<u64>` with 48-bit payload + 16-bit type tag.
- **135 opcodes**: spanning the ranges above (arithmetic, control flow, closures, actors, effects, FFI, etc.).
- **i64-tagged values**: integers, floats, booleans, nil, and unit are all represented inline — no boxing, no heap allocation for primitives.

## Memory Model

- **Per-actor heaps**: Each actor owns a 64 KB bump-allocated heap, chained on demand. Objects never move.
- **ORCA GC**: Reference counting with cycle detection, per-actor — one actor's GC never pauses another.
- **Size-class free lists**: Small allocations reuse exact-size slots; allocations over 256 bytes use a large-object space.
- **Global allocator**: `mimalloc` for all non-actor allocations (compiler, runtime, JIT buffers).

- **String interning**: Strings are interned once per module. Message passing shares interned pool handles within a node — no deep copies, though `Arc`-shared message payloads incur atomic reference-counting overhead.
- **Reference capabilities**: `iso` and `val` references are sendable without deep copies; the type system guarantees no aliasing at compile time.
- **Cross-node**: String content travels by value on the wire and is re-interned at the destination. Heap pointers, closures, and actor refs are rejected at send time.

## Benchmark methodology

The repository includes Criterion benchmarks that separate interpreter, warm-JIT, actor/runtime, cache, and supported AOT paths. Run the benchmark harness with:

```bash
cargo bench --bench bench_main
```

For any published number, record the commit SHA, CPU/OS, Rust toolchain, enabled Cargo features, workload, input size, and whether the measurement is cold interpreter, warm JIT, WASM, or native AOT. Do not compare a warmed or restricted backend against a cold/full-language path without labeling that difference.

Performance claims should also preserve semantic confidence: differential fuzzing and conformance tests are the guardrail for accepting backend optimizations.

## Comparisons

The tables below compare **execution architecture and language/runtime features**, not universal benchmark winners. Actual throughput and latency depend on workload, warm-up, backend support, allocation behavior, and hardware.

### vs Erlang/BEAM

| | Nulang | Erlang/BEAM |
|---|---|---|
| **Compilation** | Bytecode + tiered Cranelift JIT; secondary native AOT | BEAM bytecode + JIT (OTP 24+) |
| **GC** | Per-actor ORCA, no global pause | Per-process, generational |
| **Type system** | Static, HM-inferred | Dynamic |
| **Native code** | Restricted Cranelift AOT path | JIT-generated native code; HiPE is deprecated |

### vs Rust

| | Nulang | Rust |
|---|---|---|
| **Distribution** | Built-in clustering, CRDTs | Manual (gRPC, custom protocols) |
| **Supervision** | OTP-style supervision trees | Manual error handling |
| **Performance toolchain** | Bytecode + Cranelift JIT/AOT | rustc normally uses LLVM; alternative codegen backends also exist |
| **Memory safety** | Capabilities (compile-time) | Ownership + borrowing (compile-time) |

### vs Go

| | Nulang | Go |
|---|---|---|
| **Execution** | Register VM + tiered JIT; secondary AOT | Native code + goroutine scheduler + GC |
| **Concurrency** | Actors with supervision | Goroutines + channels |
| **Messaging** | Actor messages; some immutable/interned data can be shared within a node, while cross-node payloads serialize | Channels share/copy values according to Go value/reference semantics |
| **Effect tracking** | Compile-time effect rows | No effect system |

### vs Python

| | Nulang | Python |
|---|---|---|
| **Execution** | Bytecode VM + tiered JIT; optional WASM/AOT paths | CPython bytecode interpreter with version-dependent specialization/JIT work |
| **Types** | Static, full HM inference | Dynamic, optional type hints |
| **Concurrency** | Actor model, no GIL | GIL (CPython), asyncio |
