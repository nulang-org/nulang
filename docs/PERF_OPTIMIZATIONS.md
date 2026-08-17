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

### 1. ClosureCall full-frame copy (`vm.rs:3984`)
**Cost:** `ClosureCall` copies the entire 256-register array (`new_frame.regs =
self.frames[frame_idx].regs`, 2 KB) on every closure call, on top of the 2 KB
zero-init in `Frame::new`. The plain `Call` path copies only `argc` args
(`vm.rs:3493-3497`).
**Fix:** copy only the staged argument registers, matching the `Call` path.
**Trade-off:** none — identical semantics; argument registers are the only
contract.
**Risk:** low.

### 2. `behavior_id_for` allocates per send (`runtime/mod.rs:1771`)
**Cost:** every name-based `send` builds `format!(".{}", behavior)` — a String
allocation — then linear-scans the behavior table.
**Fix:** compare names without allocation: `entry.name == behavior ||
entry.name.strip_prefix(name_prefix)`.
**Trade-off:** none.
**Risk:** low.

### 3. String builder builtin
**Cost:** measured — 100k `s + "x"` appends = 3.9 s debug (O(n²) full copy,
~1.3 GB/s effective). No builder exists in `src/stdlib` or the VM.
**Fix:** a `StrBuilder` heap object behind an effect (`StrBuilder.new/push/
to_string`), plus a `stdlib::string` wrapper.
**Trade-off:** new builtin surface (additive); strings remain immutable; the
builder is the only mutable text path.
**Risk:** low-medium (new heap object type must participate in the reclamation
protocol).

### 4. Hash map builtin
**Cost:** `std.map` is an O(n) linear scan of `{key,value}` records with
copy-on-push (`map.nula`); no language-level hash table exists (Rust-side
`HashMap` is runtime-internal only).
**Fix:** a `HashMap` heap object (Rust `HashMap<Value,Value>` behind the actor
heap) exposed via an effect (`Map.insert/get/remove/contains/size`), plus a
`stdlib::map` wrapper replacing the linear-scan implementation.
**Trade-off:** new builtin surface; keys/values are counted refs (insert
retains, remove/drop releases) — must not break the ORCA reclamation protocol.
**Risk:** medium (GC integration).

### 5. Lazy, smaller actor heaps (`runtime/actor.rs:322`)
**Cost:** every actor eagerly allocates a 64 KiB bump block at spawn
(`ActorHeap::new(64 * 1024)`); measured ceiling ~16k actors/GB. Skynet bench
documents 1M actors ≈ 64 GiB.
**Fix:** defer the first block allocation until the first `alloc()`; shrink the
default block (16 KiB); growth chaining is unchanged.
**Trade-off:** a late first allocation can still fail (OOM) after spawn instead
of at spawn; memory footprint per actor drops for idle/light actors. Growth
pattern for heavy actors is unchanged (equal-size chaining).
**Risk:** low-medium; heap tests construct `ActorHeap::new(64*1024)` explicitly
and are unaffected.

### 6. Message pooling
**Cost:** every send allocates a fresh `Message` + `Arc<Vec<Value>>` payload
(`mailbox.rs:22-34`); the counting bench pays ~1.3 µs/msg.
**Fix:** a thread-confined free list of pooled `Message`/payload `Vec` boxes on
the Runtime, reused after delivery.
**Trade-off:** pooled `Vec` capacity is retained between messages (memory held,
not freed); must not pool across shards (thread confinement invariant).
**Risk:** medium.

## Deferred — with reasons

| Item | Why deferred |
|---|---|
| **Full-width 64-bit ints** | Changes the frozen `.nbc` constant encoding (format stability contract, RFC 0001) and the WASM backend's i64-tagged choice (NaN canonicalization); touches `value_layout`, JIT/AOT/WASM, Python marshal. Multi-day, format-breaking. Needs RFC + migration. |
| **Monomorphization** | New compiler pass (generic specialization at tier-up); interacts with typeclass dictionaries and closures. Multi-day. |
| **Direct-threaded interpreter** | Step-loop rewrite; Rust lacks C computed-goto; best-effort gain 10–40% on interpreter path only. Medium-large. |
| **Escape analysis / stack allocation** | Module was removed; restoring requires a new conservative analysis pass in MIR codegen. Medium-large. |
| **Copy-on-write `Array.push`** | **Unsound as designed**: `Move` does not retain (`vm.rs:4611`), so register aliases are uncounted and `ref_count == 1` does not imply uniqueness. The safe variant is an explicit mutable `ArrayBuffer` type (related to item 3). |
| **Recursion OSR** | `osr_compile_loop` hook exists but is default no-OSR; a working loop-OSR is a project. Medium-large. |
| **Inline caches (field/behavior)** | Field-access caching requires shape/record-representation work; behavior-name caching is a smaller subset of item 2. Medium. |
| **Typed-region widening** | Modeling effect opcodes in `infer_reg_types` requires per-opcode clobber analysis; medium. |
| **Wire/connection batching** | Cross-cutting distributed change; no benchmark to measure gain. Medium. |
| **String interning policy** | Pool IDs cross the wire; changing policy is semantically visible. Medium. |
| **Static effect specialization** | CPS-style compilation of multi-shot effects; large. |
| **AOT as default deployment** | Deployment/product decision, not a code change. |

## Measured baseline (before this branch)

Debug host (JIT on): 10M int loop 0.56 s; fib(30) 4.4 s; 1M builtin effect
dispatches 1.6 s; 100k string appends 3.9 s. Release: counting 757k msg/s,
ping-pong 345k msg/s, thread-ring 214k msg/s, fork-join 280k msg/s.
