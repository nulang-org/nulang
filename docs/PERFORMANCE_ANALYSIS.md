# Nulang Performance & Architecture Deep-Dive Analysis
## 28 Proposals Across 6 Tracks

**Verdict:** 20 of 28 proposals are high-value and should be pursued. 8 deferred.

> **Status update (2026-08-02):** This document was written 2026-06-25 as a
> pre-implementation evaluation. Much of Phase 1 has since shipped. All
> speedup/throughput figures quoted below ("10-100x", "~30%", "2-4x", "10x",
> "10-20%") remain proposal-era design estimates, **not measurements**, and
> should not be cited as measured results. The "no benchmark harness" claim
> that used to be here is now stale: `benches/` (6 criterion groups —
> actor, dist, gc, jit, persist, vm throughput) exists, compiles, and runs
> (`cargo bench`); a CI job now runs it on every push to `main` and commits
> raw results to `benchmarks/`. What hasn't happened yet: reconciling each
> of this document's 28 specific proposal-level claims against a matching
> measurement — the 6 bench groups are general-purpose, not a 1:1 map to
> every number below. Treat `benchmarks/*.json` as the source of truth for
> whatever it covers; everything else here is still a design-time estimate
> until it's replaced or removed. Read the status table and the "Track 1
> as built" section before treating any recommendation below as current.

### Implementation status (verified 2026-07-11)

| # | Proposal | Status | Where / notes |
|---|----------|--------|---------------|
| 1.1 | Cranelift JIT backend | **Shipped** | `src/jit/` (~7,900 lines, cranelift 0.132), tiered into `VM::step` at `src/vm.rs:1170-1216` |
| 1.2 | Type guard stripping | **Shipped** | `src/jit/typed_compiler.rs` emits guard-stripped CLIF; the live tiering path (`jit::tiered_execute_step_typed`, called from `VM::step`) recovers register types at tier-up via `typed_compiler::infer_reg_types` (conservative bytecode must-analysis) and compiles hot regions through the typed path when types are provable, falling back to scalar otherwise. Typed `IDiv`/`IMod`/`FCmpEq` always use runtime helpers to match interpreter semantics exactly |
| 1.3 | Linear scan regalloc | Deferred | Still correct — Cranelift's own regalloc is used |
| 1.4 | MLIR dialect | Deferred | No MLIR in tree |
| 1.5 | SIMD auto-vectorization | **Shipped** | `src/jit/simd_analyzer.rs` + `src/jit/simd_compiler.rs` (I64x2/F64x2/I32x4/F32x4) |
| 2.1 | Lock-free MPSC mailboxes | **Shipped differently** | `src/runtime/mailbox.rs` uses an unbounded `crossbeam::queue::SegQueue`, **not** the `ArrayQueue` recommended below — a bounded queue would force blocking or message drops, violating BEAM never-drop semantics. crossbeam's epoch-based reclamation (the Risk Register's ABA mitigation) comes with `SegQueue` |
| 2.2 | Dual-region actor heaps | **Shipped differently — LOS + grow-on-demand** | No `bumpalo` dependency — `src/runtime/heap.rs` is a hand-rolled bump allocator with size-class free lists, a large-object space (allocations over the 256-byte `Huge` threshold individually `std::alloc`'d, exact-size free-list reuse, released on `reset()`/`Drop`), and grow-on-demand chaining: bump-block exhaustion chains a fresh 64KB block instead of failing, objects never move, all blocks released on `reset()`/`Drop` (2026-07-12). No nursery/tenured split |
| 2.3 | mimalloc global allocator | **Shipped** | `mimalloc = "0.1"` in `Cargo.toml`; `#[global_allocator]` in `src/main.rs` |
| 2.4 | Static escape analysis | **Reverted** | `src/escape_analysis.rs` was added and later removed; no escape analysis in the tree today |
| 2.5 | Actor arenas | Deferred | Not present |
| 2.6 | Cache-locality scheduling | Not started | Scheduler remains single-threaded cooperative |
| 3.1 | rkyv zero-copy serialization | Not started | No rkyv dep; wire format is hand-rolled big-endian in `src/runtime/network.rs` |
| 3.2 | Delta-state CRDT replication | **Shipped** | All 8 CRDTs expose `delta_since(base)`; `CrdtManager::generate_delta_sync_ops` ships first-seen entries full and changed entries as deltas over `Packet::CrdtDeltaSync` (type 7, `CrdtDeltaOp`), applied via `apply_delta_op` (merge-only). `Runtime::sync_crdts` ships deltas every round except round 1 and every 16th round thereafter, which ship full `CrdtSync` state as the repair path |
| 3.3 | io_uring / RDMA | Deferred | Not present |
| 3.4 | Native Raft | Deferred | No Raft code |
| 3.5 | Content-addressable bytecode | Deferred | Not present |
| 4.1 | Evidence-passing style | Not started | Effects remain runtime handler-stack based (`Handle`/`Perform`/`Resume`/`Unwind`) |
| 4.2 | Linear moves for iso | **Partial** | `LinearIso` capability in `src/types.rs` with at-most-once consumption enforcement wired into `CapabilityAnalyzer` (`src/effect_checker.rs`, 2026-07-12): second use of a consumed binding is a `CapError`, sends/captures consume, conservative branch merge; exactly-once must-use is a documented follow-up. Capabilities are still erased at runtime, so no runtime move mechanism as proposed |
| 4.3 | Typestate analysis | Not started | Not present |
| 4.4 | Implicit effect returns | Not found | No corresponding transform in the current effect checker / HIR lowering |
| 5.1 | Unify actor/agent primitives | **Partial** | v0.9 AI runtime exists (`src/ai/`: OpenAI/Ollama providers, memory, pipelines, debates, supervisor) but agents are not actors with `capability llm` |
| 5.2 | Agent-aware supervision | **Partial** | `src/ai/supervisor.rs` (agent supervisor teams) exists alongside the actor `Supervisor` |
| 5.3 | Wasmtime sandboxed tools | Not started | No wasmtime dep |
| 5.4 | Agent telemetry monitors | **Partial** | `src/ai/usage.rs` tracks token usage and pricing; no general telemetry monitor |
| 6.1 | LSP inlay hints | **Shipped** | `src/lsp/mod.rs` implements inlay hints (typechecker-backed with regex fallback) |
| 6.2 | Deterministic simulation testing | Not started | No DST harness |
| 6.3 | Causal profiling | Deferred | Not present |
| 6.4 | Actor topology dashboard | Not started | Not present |

**Memory-management changes (2026-07-12, outside the 28 proposals):**
- **Intra-actor reclamation shipped.** `plan_drops` (liveness analysis in `src/mir_codegen.rs`) emits `OpCode::Drop` at conservative last-use/redefinition/branch-exit points; the `Drop` handler clears the register (idempotent); `ArrStore`/`RecS`/`FieldS` write barriers retain stored pointers and release overwritten slots; `OrcaGc::free_object` releases slot references on container free. Actor heaps no longer accumulate garbage until actor exit. This covers much of proposal 2.4's goal ("make ORCA GC effectively disappear") via liveness + barriers rather than escape analysis.
- **Refcounts downgraded to plain integers.** `OrcaHeader` counts and `GcStats` are no longer atomic — all heap/GC access runs on the single scheduler thread (network/LLM/Python threads never touch heaps; verified). The single-thread invariant is documented in SAFETY comments.
- **Heap growth shipped** — see row 2.2 above.

---

## Track 1: Native Compilation

### 1.1 Cranelift JIT Backend
| Criterion | Assessment |
|-----------|------------|
| **Impact** | Eliminates interpreter overhead (10-100x speedup for hot loops) |
| **Feasibility** | Cranelift is mature (wasmtime, rustc) |
| **Priority** | **P0 — Foundation for everything else** |
| **Effort** | 4-6 weeks |

**Recommendation:** **DO FIRST.** Highest-leverage single change. JIT can tier: interpreter for cold code, JIT for hot loops.

**Status (2026-07-11): SHIPPED.** See "Track 1 as built" below for the tiering mechanics that actually landed.

### 1.2 Static Type Guard Stripping
| Criterion | Assessment |
|-----------|------------|
| **Impact** | Eliminates ~30% of runtime overhead in numeric loops |
| **Priority** | **P0 — Bundle with 1.1** |
| **Effort** | 1-2 weeks |

**Recommendation:** **Bundle with 1.1.** Free once Cranelift backend exists.

**Status (2026-07-12): SHIPPED.** The VM's tiering entry point (`jit::tiered_execute_step_typed`) recovers register types at tier-up via `typed_compiler::infer_reg_types` (a conservative must-analysis over the enclosing function's bytecode) and compiles hot regions through the guard-stripped path when types are provable, with scalar fallback otherwise. Typed `IDiv`/`IMod`/`FCmpEq` route through runtime helpers to stay bit-identical with the interpreter. The "~30%" figure remains the module's own design estimate, unmeasured.

### 1.3 Linear Scan Register Allocation
**Recommendation:** **DEFER.** Cranelift's regalloc2 is already excellent.

### 1.4 MLIR Dialect Pipeline
**Recommendation:** **DEFER to Year 3+.** Architecturally elegant but premature.

### 1.5 SIMD Auto-Vectorization
**Recommendation:** **Pursue after 1.1.** High-value for AI inference, text processing.

**Status (2026-07-11): SHIPPED** for element-wise array loops (details below).

**Track 1 Summary: Do 1.1 + 1.2 first. Defer 1.3. Research 1.4. Do 1.5 after 1.1.**

---

## Track 1 as built (verified 2026-07-11)

The JIT backend lives in `src/jit/` (~7,900 lines across 7 files, cranelift 0.132) and is wired into the interpreter loop at `src/vm.rs:1170-1216`.

### Tiering mechanics (`src/jit/mod.rs`)

- `VM` holds `jit_session: Option<JitSession>` (`src/vm.rs:695`). A session builds the Cranelift `JITModule` for the host ISA with the `enable_simd` flag set, and registers 31 `nulang_*` runtime helpers as importable symbols.
- Each interpreted instruction pays a cheap JIT hotness/compiled-region probe first. On an actual native entry, non-reentrant Cranelift regions operate directly on the frame's `#[repr(transparent)] Value` register storage. Regions containing direct Nulang calls retain the 256-word snapshot/copy-back boundary because a nested interpreter call may grow and relocate `VM::frames`.
- Hotness is tracked per `JitSession` in flat `Vec<Vec<u32>>` rows indexed by `[module_idx][offset]`, avoiding a hash lookup on each cold interpreted step. `HOT_THRESHOLD = 1000`: a region becomes eligible after 1,000 interpreted hits.
- Region discovery scans at most **500 instructions**. Small straight-line fragments are rejected below `STRAIGHT_LINE_MIN = 8` because repeated JIT entry/exit can cost more than interpretation. Backward-edge loops are treated differently: the scanner can include the loop's branches/back-edge so the hot loop executes as one native region, with exits back to the interpreter when control leaves the region.
- Compiled functions are cached per `(module_idx, offset)`; a region is compiled at most once per session.

### Compiled-function ABI

```rust
pub type JitFunctionPtr = extern "C" fn(*mut u64, *const u64);
```

Argument 1 is the 256-entry register file (read/write), argument 2 the module's constant pool pre-converted to raw NaN-boxed bits (`constants_to_jit_bits`, `src/vm.rs:485`). JIT function pointers are obtained by transmuting the finalized `*const u8`; bytecode must not be mutated while JIT code is executing.

### Scalar path (`src/jit/compiler.rs`)

MVP opcode subset (50 opcodes): `Nop`/`Halt`, `Const0/1/2/M1`/`ConstU`, `Load`/`Store`/`Move`/`Swap`/`Dup`, integer and float arithmetic, integer/float compares, `Not`/`And`/`Or`, `Jmp`/`JmpT`/`JmpF`, `IToF`/`FToI`, `DbgPrint`, `Ret`/`RetVal`, `ArrLoad`. Everything else (actors, effects, FFI, Python, strings) forces interpretation.

Arithmetic lowers to calls to 31 `#[no_mangle] extern "C"` helpers in `src/jit/runtime.rs`. These are NaN-tag aware via the canonical layout in `src/value_layout.rs` (`sext48`, `tag_int`, `PAYLOAD_MASK`). Integer `idiv`/`imod` by zero return `nil` instead of trapping; float `fdiv` is raw IEEE-754. `fcmp_eq` compares with `f64::EPSILON` tolerance.

### Typed path (`src/jit/typed_compiler.rs`)

Given `TypeMetadata` (register → `KnownType::{Int, Float, Bool, Unknown}`), the typed compiler emits direct CLIF (`iadd`, `fadd`, …) instead of helper calls, stripping NaN-tag manipulation for statically known operands and falling back to helpers for `Unknown`. **Live**: the tiering entry point is `jit::tiered_execute_step_typed` (called from `VM::step`, `src/vm.rs:1170-1216`); at tier-up it runs `typed_compiler::infer_reg_types` — a conservative forward must-analysis over the enclosing function's bytecode — and compiles the hot region through the guard-stripped path when at least one register type is provable, falling back to the scalar `compile_region` on absent/empty metadata or compile error. Typed `IDiv`/`IMod`/`FCmpEq` always emit runtime-helper calls (never raw `sdiv`/`srem`/`fcmp`) to stay bit-identical with the interpreter (div-by-zero → `nil`, epsilon float equality).

### SIMD path (`src/jit/simd_analyzer.rs`, `src/jit/simd_compiler.rs`)

`JitSession::compile_region_simd` runs `analyze_region` first. The analyzer recognizes three loop shapes — `ElementWiseBinop` (`c[i] = a[i] + b[i]`), `ElementWiseUnary` (`b[i] = -a[i]`), `ElementWiseCmp` (`c[i] = a[i] < b[i]`) — requiring an `ArrLoad`→arithmetic→`ArrStore` chain on a single induction variable that steps by 1, a determinable trip count (`ArrLen` bound or constant), a uniform element type, no calls in the body, and no control flow beyond the back-edge.

The SIMD compiler emits 128-bit vectors — `I64x2`/`F64x2` (2-wide) and `I32x4`/`F32x4` (4-wide) — as a scalar prefix loop, a SIMD body, and a scalar epilogue (`SimdWidth::Width8`/`I16x8` is reserved, not implemented). Compilation requires a compile-time trip-count hint and host support (`is_simd_supported()`: SSE2 on x86_64, always true on aarch64); otherwise it falls back to the typed scalar compiler. On a SIMD compile error the session falls back to the plain scalar `compile_region`.

### Testing

71 `#[test]`s under `src/jit/` (35 in `tests.rs`, 3 in `compiler.rs`, 15 in `simd_analyzer.rs`, 10 in `simd_compiler.rs`, 8 in `typed_compiler.rs`) plus 2 VM-level regression tests in `src/vm.rs` (`test_jit_hot_loop_matches_interpreter`, `test_jit_hot_loop_with_early_exit_branch`) and a typed-path test in `src/integration_tests.rs` (`test_jit_typed_guard_stripping_hot_function`) that assert JIT-compiled hot loops produce exactly the interpreter's result. There is **no performance benchmark harness** anywhere in the repository — all speedup numbers in this document are estimates.

### Not implemented (do not claim)

No whole-function or ahead-of-time compilation; no inlining; no deoptimization or on-stack replacement beyond re-entering the interpreter at region boundaries; no JIT support for actor, effect, FFI, or Python opcodes; no control flow across region boundaries; no 8/16-bit SIMD widths; no recorded benchmark numbers.

---

## Track 2: Memory Management & Runtime Concurrency

### 2.1 Lock-Free CAS MPSC Actor Mailboxes
| Criterion | Assessment |
|-----------|------------|
| **Impact** | Eliminates mutex contention (up to 10x throughput improvement) |
| **Priority** | **P0 — Critical for multi-core scaling** |
| **Effort** | 2-3 weeks |

**Recommendation:** **DO IMMEDIATELY.** Highest-ROI change in Track 2. Use `crossbeam::queue::ArrayQueue`.

**Status (2026-07-11): SHIPPED with a different structure.** `src/runtime/mailbox.rs` uses an unbounded `crossbeam::queue::SegQueue`, not `ArrayQueue`: a bounded queue forces a choice between blocking senders and dropping messages, both of which violate BEAM never-drop semantics. Push always succeeds; reclamation uses crossbeam's epoch-based GC.

### 2.2 Dual-Region Actor Heaps (LOS Split)
**Recommendation:** **DO.** Natural evolution of the existing per-actor heap.

**Status (2026-07-12): LOS SHIPPED.** There is no `bumpalo` dependency — `src/runtime/heap.rs` is a hand-rolled bump allocator with per-size-class intrusive free lists and ORCA headers, plus a large-object space: allocations over the 256-byte `Huge` threshold are individually `std::alloc`'d outside the 64KB backing block, with exact-size free-list reuse and release on `reset()`/`Drop`. No nursery/tenured split.

### 2.3 Global Memory Arena via mimalloc
**Recommendation:** **DO RIGHT NOW.** Literally a one-line change. Instant 10-20% win.

**Status (2026-07-11): SHIPPED.** `mimalloc = "0.1"` is the `#[global_allocator]` in `src/main.rs`. The "10-20%" figure was never measured.

### 2.4 Static Escape Analysis for Stack Allocation
**Recommendation:** **DO after 1.1.** Key to making ORCA GC effectively disappear.

**Status (2026-07-11): REVERTED.** `src/escape_analysis.rs` was implemented and subsequently removed (tests and references cleaned up); no escape analysis exists in the tree today.

### 2.5 Actor Arenas (Allocation Pooling)
**Recommendation:** **DEFER.** Power-user feature; covered by 2.2 and 2.4.

### 2.6 Cache-Locality Aware Heterogeneous Scheduling
**Recommendation:** **Pursue after 2.1.** Important for Intel/ARM hybrid CPUs.

**Track 2 Summary: Do 2.1, 2.3 immediately. Do 2.2, 2.4 next. Defer 2.5. Do 2.6 after 2.1.**

---

## Track 3: Distributed Mesh & Consensus

### 3.1 Zero-Copy Serialization via rkyv
**Recommendation:** **DO.** Eliminates 50%+ of distributed message latency.

### 3.2 Delta-State & Op-Based CRDT Replication (CmRDT)
**Recommendation:** **DO.** 10-100x bandwidth reduction for large CRDTs.

**Status (2026-07-12): SHIPPED (delta-state).** All 8 CRDTs expose `delta_since(base)`; `CrdtManager::generate_delta_sync_ops` ships deltas over `Packet::CrdtDeltaSync` (type 7) via `sync_crdts_delta` in `src/runtime/distributed.rs`, with full-state `CrdtSync` retained as the join/repair path. Op-based (CmRDT) replication was not implemented.

### 3.3 Kernel-Bypass Networking (io_uring & RDMA)
**Recommendation:** **DEFER.** Platform-specific; significant complexity increase.

### 3.4 Native Raft Consensus Engine
**Recommendation:** **DEFER to Phase 4.** CRDTs cover 80% of distributed state needs.

### 3.5 Content-Addressable Bytecode Functions
**Recommendation:** **DEFER to Year 3+.** Unison-style paradigm shift; enormous complexity.

**Track 3 Summary: Do 3.1, 3.2. Defer 3.3, 3.4, 3.5.**

---

## Track 4: Type System Synergy

### 4.1 Evidence-Passing Style / Capability Inlining
**Recommendation:** **DO after 1.1.** Key optimization for algebraic effects performance.

### 4.2 Linear Type Move Semantics for iso Capabilities
**Recommendation:** **DO IMMEDIATELY.** Type system change for zero-cost actor messaging.

### 4.3 Typestate Analysis for Temporal Contracts
**Recommendation:** **Pursue after core type system stabilization.** Major differentiator.

### 4.4 Implicit Effect Return Fallbacks
**Recommendation:** **DO.** Trivial AST transformation; good ergonomics payoff.

**Track 4 Summary: Do 4.2 immediately. Do 4.1 after Cranelift. Do 4.3 after type system stabilization. Do 4.4 anytime.**

---

## Track 5: AI Agent Native Infrastructure

### 5.1 Complete Unification of Actor and Agent Primitives
**Recommendation:** **DO.** Aligns with v2.0 spec: agents become actors with `capability llm`.

### 5.2 Agent-Aware Supervision Trees
**Recommendation:** **DO.** Essential for production AI systems. LLM APIs are unreliable.

### 5.3 Sandboxed Tool Execution via Embedded Wasmtime
**Recommendation:** **DO IMMEDIATELY.** Security requirement, not a feature.

### 5.4 Agent Telemetry Monitors
**Recommendation:** **DO after core AI runtime.** Important for cost management.

**Track 5 Summary: Do 5.1, 5.3 immediately. Do 5.2 next. Do 5.4 after core AI runtime.**

---

## Track 6: Systems Observability & Verification

### 6.1 Real-Time LSP Inlay Hints
**Recommendation:** **DO IMMEDIATELY.** Most impactful developer experience feature.

### 6.2 Deterministic Simulation Testing (DST)
**Recommendation:** **DO.** How FoundationDB achieved legendary reliability.

### 6.3 Causal Profiling Infrastructure
**Recommendation:** **DEFER.** Research tool; profilers + DST cover most needs.

### 6.4 Visual Actor Topology Dashboard
**Recommendation:** **DO as a side project.** Low-risk, high-fun, great for demos.

**Track 6 Summary: Do 6.1 immediately. Do 6.2 next. Do 6.4 as side project. Defer 6.3.**

---

## Revised Implementation Matrix

### Phase 1: Native Speed Foundation (Weeks 1-8)
| # | Proposal | Effort | Depends On |
|---|----------|--------|------------|
| 2.3 | mimalloc global allocator | 1 day | — |
| 2.1 | Lock-free CAS MPSC mailboxes | 3 weeks | — |
| 1.1 | Cranelift JIT Backend | 6 weeks | — |
| 1.2 | Type guard stripping / unboxing | 2 weeks | 1.1 |
| 4.2 | Linear type moves for iso | 3 weeks | — |
| 6.1 | LSP Inlay Hints | 4 weeks | — |

### Phase 2: Memory & Concurrency (Weeks 9-16)
| # | Proposal | Effort | Depends On |
|---|----------|--------|------------|
| 2.2 | Dual-region actor heaps | 3 weeks | — |
| 2.4 | Static escape analysis | 4 weeks | 1.1 |
| 2.6 | Cache-locality scheduling | 5 weeks | 2.1 |
| 4.1 | Evidence-passing style | 5 weeks | 1.1 |
| 1.5 | SIMD auto-vectorization | 4 weeks | 1.1 |
| 6.2 | Deterministic Simulation Testing | 7 weeks | — |

### Phase 3: Distributed & AI (Weeks 17-28)
| # | Proposal | Effort | Depends On |
|---|----------|--------|------------|
| 3.1 | rkyv zero-copy serialization | 3 weeks | — |
| 3.2 | Delta-state CRDT replication | 5 weeks | 3.1 |
| 5.1 | Unify actor/agent primitives | 2 weeks | — |
| 5.3 | Wasmtime sandboxed execution | 4 weeks | — |
| 5.2 | Agent-aware supervision | 3 weeks | 5.1 |
| 4.3 | Typestate analysis | 7 weeks | 4.2 |
| 6.4 | TUI topology dashboard | 3 weeks | — |

### Phase 4: Advanced (Weeks 29-40+)
| # | Proposal | Effort | Depends On |
|---|----------|--------|------------|
| 3.4 | Native Raft consensus | 10 weeks | 3.2 |
| 4.4 | Implicit effect returns | 1 week | — |
| 5.4 | Agent telemetry monitors | 3 weeks | 5.2 |
| 1.3 | Linear scan regalloc (if needed) | 4 weeks | 1.1 |
| 2.5 | Actor arenas (if needed) | 4 weeks | 2.2 |

### Deferred (Research / Year 3+)
- 1.3: Linear scan regalloc (use Cranelift's default)
- 1.4: MLIR dialect pipeline
- 2.5: Actor arenas
- 3.3: io_uring / RDMA
- 3.5: Content-addressable bytecode
- 6.3: Causal profiling

---

## Critical Path Analysis

```
Cranelift JIT (1.1, 6w) → Type guard stripping (1.2, 2w) → Evidence-passing (4.1, 5w) → Escape analysis (2.4, 4w)
```

**Total: 17 weeks for full native-speed + zero-cost effects + stack allocation.**

Parallel tracks:
- **Type system:** Linear moves (4.2) can proceed in parallel with Cranelift (1.1)
- **Memory:** Lock-free mailboxes (2.1) + mimalloc (2.3) are independent
- **Distributed:** rkyv (3.1) + delta CRDTs (3.2) are independent of native compilation
- **Observability:** LSP (6.1) is independent; DST (6.2) needs stable runtime

**Total timeline to Phase 3 completion: ~28 weeks (7 months) with 2 engineers.**

---

## Risk Register

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| Cranelift JIT has codegen bugs | Medium | High | Maintain interpreter as fallback |
| Lock-free mailbox ABA bug | Low | Critical | Use `crossbeam::epoch` |
| Escape analysis unsoundness | Medium | Critical | Conservative analysis; fall back to heap |
| rkyv endianness across platforms | Medium | High | Standardize little-endian wire format |
| LSP performance on large files | Medium | Medium | Incremental type checking; caching |
| Wasmtime sandbox escape | Low | Critical | Keep Wasmtime updated; minimal WASI |

---

## Final Verdict

**20 of 28 proposals should be pursued.** The deferred 8 are premature optimizations, research projects, or platform-specific. The 20 recommended proposals form a coherent, phased roadmap that transforms Nulang from an interpreted research language into a production-grade, native-speed distributed actor platform.

**Sequence matters: JIT first, then memory, then distributed, then AI, then advanced type system.**
