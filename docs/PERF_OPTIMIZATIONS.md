# Performance Optimization Audit & Trade-offs

Branch: `perf/optimizations`. Audit date: 2026-08-17.
Every item below was verified against the source tree before any change was made.

## Methodology

For each proposed optimization: (a) confirm the cost exists in the current code,
(b) confirm the proposed fix does not already exist, (c) evaluate the trade-off,
(d) implement only if safe and self-contained. Items whose trade-offs are
unacceptable, or whose effort/risk is disproportionate, are deferred with the
reason recorded here.

## Already exists — no action needed

| Claim | Finding |
|---|---|
| Step-limit env re-read per step | Cached in a `OnceLock` (`vm.rs:2596`) — already fixed |
| Single-shot effect fast path | `SingleShotState` avoids heap allocation on `PerformDirect` single-shot bindings |
| Heap free lists / size classes | `heap.rs` free lists + exact-size reuse exist |
| Scheduler micro-batching | L1-retention batching exists (`runtime/mod.rs:2210`) |
| Atomic-free refcounts | Plain integers by thread-confinement — a design feature, keep |
| Interning dedup on the wire | `intern_wire_strings` dedups by content (`distributed.rs:2468`) |

## Implemented on this branch

### 1. `behavior_id_for` allocates per send (`runtime/mod.rs:1771`)
**Cost:** every name-based `send` builds `format!(".{}", behavior)` — a String
allocation — then linear-scans the behavior table.
**Fix:** allocation-free suffix match (`strip_suffix` + `ends_with('.')`) in
`behavior_id_for` and `distributed::try_lookup_content_hash`.
**Trade-off:** none.
**Risk:** low. **Shipped.**

### 2. String builder builtin
**Cost:** measured — 100k `s + "x"` appends = 3.9 s debug (O(n²) full copy).
**Fix:** `StrBuilder` effect — mutable growable buffer (Raw heap bytes,
capacity doubling, amortized O(n)). `new`/`push`/`to_string`/`len`/`reset`,
wired into standalone + runtime dispatch, `stdlib::string` wrappers.
**Risk:** low-medium. **Shipped** (4 tests).

### 3. Hash map builtin
**Cost:** `std.map` is an O(n) linear scan; no language-level hash table.
**Fix:** `Map` effect — self-contained open-addressed hash table in Value
slots (content-based string keys, load-factor-0.5 growth, tombstone reuse).
Integrated with ORCA: keys/values retained on insert, released on
remove/overwrite, and `TypeTag::Map` added to `free_object` slot-release.
**Risk:** medium (reclamation protocol). **Shipped** (5 tests).

### 4. Actor heap density (`runtime/actor.rs`)
**Cost:** every actor eagerly allocated a 64 KiB bump block at spawn.
**Fix:** default block 16 KiB — ~4× density (≈64k actors/GB). (Full lazy
first-block allocation deferred — see below.)
**Risk:** low. **Shipped.**

## Considered and reverted

### ClosureCall full-frame copy (`vm.rs`, `core_vm`)
`ClosureCall` copied all 256 registers per call. **Audit result:** this is a
**legacy opcode** — `mir_codegen` emits `OpCode::Call` for closures too (which
already stages only `argc` args from r0). `ClosureCall` is only used by
hand-built bytecode (`http_server.rs`, two vm.rs tests) that rely on
copy-all to pass args in non-r0 registers. The optimization targeted dead
code and broke the hand-built sites; **reverted** on the audit finding.

## Deferred — with reasons

| Item | Why deferred |
|---|---|
| **Full-width 64-bit ints** | Changes the frozen `.nbc` constant encoding (format stability contract, RFC 0001) and the WASM backend's i64-tagged choice (NaN canonicalization); touches `value_layout`, JIT/AOT/WASM, Python marshal. Multi-day, format-breaking. Needs RFC + migration. |
| **Monomorphization** | New compiler pass (generic specialization at tier-up); interacts with typeclass dictionaries and closures. Multi-day. |
| **Direct-threaded interpreter** | Step-loop rewrite; Rust lacks C computed-goto; best-effort gain 10–40% on interpreter path only. Medium-large. |
| **Escape analysis / stack allocation** | Module was removed; restoring requires a new conservative analysis pass in MIR codegen. Medium-large. |
| **Copy-on-write `Array.push`** | **Unsound as designed**: `Move` does not retain (`vm.rs`), so register aliases are uncounted and `ref_count == 1` does not imply uniqueness. The safe variant is an explicit mutable `ArrayBuffer` type. |
| **Recursion OSR** | `osr_compile_loop` hook exists but is default no-OSR; a working loop-OSR is a project. Medium-large. |
| **Inline caches (field/behavior)** | Field-access caching requires shape/record-representation work. Medium. |
| **Typed-region widening** | Modeling effect opcodes in `infer_reg_types` requires per-opcode clobber analysis. Medium. |
| **Wire/connection batching** | Cross-cutting distributed change; no benchmark to measure gain. Medium. |
| **String interning policy** | Pool IDs cross the wire; changing policy is semantically visible. Medium. |
| **Static effect specialization** | CPS-style compilation of multi-shot effects; large. |
| **AOT as default deployment** | Deployment/product decision, not a code change. |
| **Message pooling** | Requires careful analysis of the payload refcount lifecycle (payload Values carry counted sender-heap refs). Breaking the ORCA reclamation protocol is an explicit "do not break" area. Modest gain (~a Vec alloc per send); high blast radius. |
| **"Already scheduled" scheduler flag** | Flag lifecycle across work-stealing + cross-shard `EnqueueActor` is a stuck-flag/deadlock risk. Scheduler is already tuned (757k msg/s counting). |
| **Per-step guard trimming** | The step-limit check is already a cached `OnceLock` + compare per step — negligible cost. Changing step-count semantics risks continuation metadata and step-limit tests. |

## Pre-existing upstream issue (not on this branch)

`**` with a 48-bit-overflowing exponent diverges: interpreter wraps (returns
`0`), AOT returns `nil`. Caused by the upstream revert `2f3ef93` that made
the interpreter lenient while AOT stayed strict. Reproduces on the base
commit — a real correctness bug (interp/AOT semantic asymmetry), out of
scope for this perf branch (needs the 64-bit-int work or a deliberate
interp/AOT alignment decision).

## Measured baseline (before this branch)

Debug host (JIT on): 10M int loop 0.56 s; fib(30) 4.4 s; 1M builtin effect
dispatches 1.6 s; 100k string appends 3.9 s. Release: counting 757k msg/s,
ping-pong 345k msg/s, thread-ring 214k msg/s, fork-join 280k msg/s.
