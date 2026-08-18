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

## Considered, measured, and rejected

### Lazy frame-register zeroing (`vm.rs` `step_call`)
`Frame::new` initializes `[Value::nil(); 256]` (2 KB) on every call, and
`Call`/`ClosureCall` are **not JIT-compilable** (only `Ret`/`RetVal` are;
`find_compilable_region` stops at a call). So call-bound code (fib, closures,
behavior dispatch) pays the 2 KB frame zero + the interpreter dispatch on
every call, and never tiers up.

**Measured:** microbenchmarked the frame machinery (2 KB init + push + pop ×
2.7M) at **72 ms**; release fib(30) is **1.27 s** (~470 ns/call). The frame
init share is **~27 ns/call (~6%)**; lazy-zeroing the used prefix would save
~3%. To avoid the full-array init, a frame must either use `MaybeUninit`
(UB on the never-read tail) or reuse stale-tail frames — and stale-tail
values can be heap pointers from freed objects, which continuation
snapshots / hibernation serialization / DAP would walk as dangling refs.
**Rejected: ~3% in release, no safe implementation. The interpreter dispatch
(not the frame zero) dominates call-bound code.**

**The real fix is JIT-compiling `Call`** (native call ABI between compiled
regions) — that eliminates both the frame zero and the per-instruction
interpreter dispatch for hot calls. Large, needs its own audit. See the
JIT-call note below.

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

## JIT-compiling `Call` (next high-impact item — dedicated effort)

`Call`/`ClosureCall` are not JIT-compilable, so hot call-graphs (fib,
closures, recursion) never tier up and run fully interpreted (~470 ns/call
for fib in release, of which the frame zero is ~6%). The high-impact fix is a
native call between compiled regions so hot callees run natively.

**Audit findings:**
- Return convention: callee writes `regs[0]` (Ret) or `regs[op1]` (RetVal);
  the interpreter copies it to the caller's `return_dst` and pops.
- The region ABI is `extern "C" fn(regs: *mut u64, consts: *const u64)`; there
  is no per-frame isolation in native code — a native callee sharing the regs
  buffer clobbers the caller's live regs (needs caller-save/liveness).
- **Suspension correctness is the blocker.** A native (or helper-run) callee
  that suspends mid-execution must not be re-run from the call start (that
  double-executes pre-suspend side effects). The only sound approach is a
  static, transitive `may_suspend` gate: compile/native-call a callee only if
  it provably never suspends (no suspending effect opcode in its transitive
  call graph), else fall back to the interpreter (which handles suspension
  correctly from the start).

**Frozen-format constraint (discovered, do not re-violate):** adding a new
opcode (e.g. a `CallDirect`) is an **additive format change** — a freshly
compiled `.nbc` would carry a new opcode value under a `BYTECODE_VERSION=1`
header, which a conforming v1 runtime rejects with `UnknownOpcode`
(`format/nbc.rs`: the instruction stream is "coupled to the frozen opcode
values"; `migrate.rs` is "the sole legal home for format upgrades").
Minting an opcode requires `BYTECODE_VERSION`→2 + a `migrate` entry + an RFC
(the language version is `1.0.0-frozen` and moves only on RFC-ratified
change) — a governance decision, not a perf-branch one. **The direct-call
target must be recovered without a new opcode.**

**Design (no format change):**
1. **`may_suspend` vector, VM-side (not in frozen `CodeModule`):** computed at
   compile time from MIR (whose `RValue::Call { func: FuncRef }` distinguishes
   direct `Index(idx)` from indirect `Local`), as a transitive fixed point
   over the direct call graph + suspending effect opcodes. Stored per module
   keyed by function-table index, threaded to the JIT (e.g. `jit_session`
   side-table) so the region compiler can gate a native call on the callee.
2. **Region-compiler peephole:** recover the direct callee from the existing
   `load_constant(FUNC_VALUE_REG, idx)` + `Call(FUNC_VALUE_REG, argc, dst)`
   sequence mir_codegen already emits for `FuncRef::Index` (FUNC_VALUE_REG =
   254). When recognized and the callee is `!may_suspend`, emit a native call
   (or a helper that runs the non-suspending callee to completion, then writes
   the result to `regs[dst]`), preserving the caller's regs buffer. Indirect
   (`FuncRef::Local`) and suspending callees stay on the interpreter path.
3. Follow-up (if the native-call win warrants it): caller-save of live-across-
   call regs via a bounded liveness pass over the compiled region, to allow
   native callees that share the regs buffer.

The first slice (safe, correct, testable) is the peephole + `may_suspend`
gate emitting a helper that runs a provably-non-suspending callee to
completion. This compiles the caller's whole body around direct calls (fib:
arg computes, adds, branch native; recursive calls interpreted), with the
recursion tiering up as the callee's own region compiles.

**Measured: the yield-at-Call variant regresses.** A first attempt included
direct non-suspending calls in regions and yielded to the interpreter at each
call (no native call, no re-entrancy — correct by construction). A call-heavy
loop regressed ~30% (2.21s → 2.86s) because the per-iteration yield/re-entry
overhead exceeded the small native-block savings — the same trap the existing
`STRAIGHT_LINE_MIN` guard documents. Only a re-entrant native-call helper
(runs the non-suspending callee to completion in one step, thread-local
save/restore, runtime reg-254 verification) delivers the win, and that is
high-risk (frame management + nested JIT). The `may_suspend` analysis +
direct-call peephole (the foundation) are landed and tested; the native-call
helper is the remaining high-risk slice.

## Correctness fix landed on this branch

Int `**` overflow diverged across backends: the interpreter's `step_ipow`
wraps on 48-bit overflow, but the shared JIT/AOT helper `nulang_pow` was
checked (returned `nil` + recorded an error). Fixed by making `nulang_pow`
mirror `step_ipow` (wrap, negative exp → nil), and by excluding `BinOp::Pow`
from AOT unboxed compilation (`is_all_int`) — Pow was missing from the
nil-producing exclusion, so an all-Int fn compiled unboxed, fell through the
unboxed binop match to the tagged helper with raw operands, and returned `0`
for non-overflow pow. See commits `db5a4fd` + `39420ef`.

## JIT-compiling `Call` (next high-impact item)

`Call`/`ClosureCall` are not JIT-compilable, so hot call-graphs (fib,
closures, recursion) never tier up and run fully interpreted
(~470 ns/call for fib in release, of which the frame zero is ~6%). The
high-impact fix is a native call ABI between compiled regions: stage args,
call the callee's compiled region in the shared 256-reg buffer (caller-save),
return through the interpreter trampoline for cold callees. This eliminates
both the per-call frame zero and the interpreter dispatch for hot calls.
Scope: Cranelift `call` lowering, callee-save discipline, tier-up of the
callee region, and fallback-to-interpreter for un-compiled callees. Multi-day;
needs its own audit.

## Measured baseline (before this branch)

Debug host (JIT on): 10M int loop 0.56 s; fib(30) 4.4 s; 1M builtin effect
dispatches 1.6 s; 100k string appends 3.9 s. Release: counting 757k msg/s,
ping-pong 345k msg/s, thread-ring 214k msg/s, fork-join 280k msg/s.
