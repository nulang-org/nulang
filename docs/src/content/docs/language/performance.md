---
title: Performance
description: Nulang's performance architecture — JIT compilation, native AOT, WASM backend, SIMD, and zero-copy execution.
---
import JitDemo from '../../../components/animations/JitDemo.astro';

## JIT Tiering

<JitDemo />

Nulang's bytecode VM uses hot-counter tiering to compile frequently-executed code paths:

- **Threshold**: After 1,000 invocations at a given PC, the VM triggers Cranelift compilation.
- **Region compilation**: The compiler scans up to 500 instructions, stopping at unsupported opcodes or `Ret`.
- **Typed compilation**: When register types are statically provable (via `TypeMetadata`), the typed compiler strips NaN-tag guards, emitting unboxed integer and float operations directly.
- **SIMD auto-vectorization**: Element-wise loops on arrays are detected by the SIMD analyzer and compiled to `I64x2`, `F64x2`, `I32x4`, or `F32x4` vector instructions with scalar prefix/epilogue.

Warm-up behavior: the first 1,000 iterations of a hot loop run in the interpreter. Once the threshold is crossed, the compiled region replaces interpretation for subsequent iterations.

## Native AOT

The `--backend native` flag compiles Nulang directly to native code:

- **Pipeline**: Source → AST → HIR → MIR → Cranelift CLIF → native object code.
- **Unboxed operations**: Compile-time type metadata (`src/type_metadata.rs`) enables unboxed integer and float arithmetic — no NaN-tagging overhead in compiled code.
- **Zero VM overhead**: AOT-compiled functions run natively without interpreter dispatch or frame management.

:::caution[Limitation]
`--backend native` compiles a **pure-functional subset only** — no effects, no actors, and no FFI (the compiler errors name the unsupported construct). Use the default bytecode backend for full-language programs.
:::

## WASM Backend

The WASM backend (`--backend wasm`, requires `--features wasm-backend`) compiles MIR to WebAssembly:

- **Compiler**: `wasm-encoder` emits `.wasm` modules with i64-tagged values to avoid WASM NaN canonicalization.
- **Runtime**: Wasmtime host runtime with 4 GiB guard pages, Cranelift speed optimizations (inlining enabled), and SIMD support.
- **AOT compilation**: `wasmtime compile` produces `.cwasm` files for instant startup — no JIT warm-up on the client side.
- **SIMD lowering**: MIR array operations lower to WASM SIMD instructions via raw byte emission.

:::caution[Limitation]
The WASM backend supports **IO.print/read only** — no user-defined effect handlers and no actor mailbox. (The experimental `wasmfx-backend` feature adds WasmFX stack-switching for suspending effects such as `LLM.ask`, `Signal.wait`, and `ReceiveWait`.)
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

## Comparisons

### vs Erlang/BEAM

| | Nulang | Erlang/BEAM |
|---|---|---|
| **Compilation** | JIT + native AOT | BEAM JIT (as of OTP 24) |
| **GC** | Per-actor ORCA, no global pause | Per-process, generational |
| **Type system** | Static, HM-inferred | Dynamic |
| **Native code** | AOT via `--backend native` | HiPE (deprecated) |

### vs Rust

| | Nulang | Rust |
|---|---|---|
| **Distribution** | Built-in clustering, CRDTs | Manual (gRPC, custom protocols) |
| **Supervision** | OTP-style supervision trees | Manual error handling |
| **Performance** | Cranelift (same backend as Rust via `cranelift-codegen`) | LLVM (via rustc) |
| **Memory safety** | Capabilities (compile-time) | Ownership + borrowing (compile-time) |

### vs Go

| | Nulang | Go |
|---|---|---|
| **Execution** | Register VM + JIT + AOT | Goroutine scheduler + GC |
| **Concurrency** | Actors with supervision | Goroutines + channels |
| **Messaging** | Zero-copy handles (within node) | Channel copy |
| **Effect tracking** | Compile-time effect rows | No effect system |

### vs Python

| | Nulang | Python |
|---|---|---|
| **Execution** | Compiled (VM + JIT + AOT) | Interpreted (CPython) |
| **Types** | Static, full HM inference | Dynamic, optional type hints |
| **Concurrency** | Actor model, no GIL | GIL (CPython), asyncio |
| **Startup** | Sub-millisecond (AOT) | ~100 ms (CPython import) |
