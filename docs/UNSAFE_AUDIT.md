# Unsafe-Code Audit

Branch: `audit/unsafe-analysis` (based on `origin/main` @ 654910f).

Scope: all `unsafe` occurrences under `src/` (`grep -rn "unsafe" src/` → 616
hits at audit time, including tests and doc comments). Each non-test site was
classified as:

- **sound-with-proof-comment** — an accurate `SAFETY:`/`# Safety` justification
  is present at the site or on the enclosing `unsafe fn`.
- **sound-by-convention** — correct only because of a project-wide invariant
  (single-threaded actor heaps, null-terminated heap strings, 48-bit pointer
  payloads) that is not enforced by the type system.
- **suspicious** — a concrete soundness hazard or a missing guard.

## Per-file inventory (non-test sites)

| File | unsafe hits | Classification | Notes |
|---|---|---|---|
| `src/runtime/heap.rs` | 64 | mostly sound-with-proof-comment | intrusive free/live lists, header arithmetic; `unsafe fn free`/`header_of` carry proper contracts |
| `src/vm.rs` | 53 | mixed — **suspicious** | `Value::from_raw`/`from_bits` safe constructors; `Value::ptr` 48-bit truncation; `CStr::from_ptr` on heap strings |
| `src/runtime/gc.rs` | 75 | sound-with-proof-comment | ORCA barriers; pointer methods are `unsafe fn` with contracts |
| `src/runtime/callbacks.rs` | 49 | sound-by-convention | `BytecodeRuntimeCallbacks` Send/Sync now documented; `RuntimeVmCallbacks` derefs rely on VM-owned pointers |
| `src/jit/runtime.rs` | 91 | sound-by-convention | fat-pointer transmutes, `'static` constant pool, extern "C" helpers trusting raw u64 values |
| `src/runtime/orca_cycle.rs` | 35 | sound-with-proof-comment | `ForeignRefNode::is_alive` relies on free-notification protocol |
| `src/ffi/c_api.rs` | 24 | sound-with-proof-comment | null checks present on all entry points |
| `src/ffi/marshal.rs` | 29 | sound-by-convention | fn-pointer transmutes; pointer packing (fixed, see F3) |
| `src/ffi/native.rs` | 21 | sound-with-proof-comment | `libloading` wrapper; Send/Sync justified |
| `src/runtime/mod.rs` | 22 | sound-by-convention | scattered heap/callback derefs |
| `src/aot/mod.rs` | 49 | sound-by-convention | same patterns as `jit/runtime.rs` (raw values → pointers) |
| `src/runtime/heap_serialize.rs` | 6 | sound-by-convention | trusts recorded payload sizes; see F7 |
| `src/main.rs`, `src/runtime/mailbox.rs`, `src/wasm_runtime.rs`, others | ≤5 each | sound-with-proof-comment | localized, documented |

## Findings (ranked by severity)

### F1 — HIGH — safe `Value::from_raw` / `Value::from_bits` construct arbitrary tagged pointers
`src/vm.rs:1385` (`from_raw`), `src/vm.rs:1395` (`from_bits`).
Both are **safe** `pub fn`s that wrap any `u64` into a `Value`. Any safe code
can therefore mint a `TAG_PTR` value pointing at an arbitrary address, which is
later dereferenced (header read, `CStr::from_ptr`, slot walks) inside `unsafe`
blocks that trust the tag. This undermines every SAFETY argument in the
runtime ("ptr came from our heap") because the value may not have.
`from_raw` even carries a `# Safety` doc section despite being a safe fn.
**Status:** CLOSED (2026-09-14) — both constructors are now `pub unsafe fn
from_raw_unchecked` / `pub unsafe fn from_bits_unchecked` (src/vm.rs), with a
boundary-specific `SAFETY:` justification audited at every call site (trusted
VM/JIT/AOT round-trip, WASM/runtime ABI, persistence reconstruction,
Python/FFI/C boundary, test/fuzz harnesses). Externally supplied bits must
enter through the fail-closed `try_from_untrusted_bits` decoder, which rejects
pointer-tagged values before they can reach heap/GC dereference paths. The
`NulangValue` C ingress remains covered by the C API's unsafe caller contract.
See "F1b — CLOSED" below for the companion `Value::ptr` provenance closure
(issue #186). Regression coverage: `tests/raw_value_provenance.rs` and a
`compile_fail` doctest on `Value::ptr`.
**Recommended fix:** ~~mark both `pub unsafe fn`~~ applied as described above.

### F2 — HIGH — `Value::ptr` truncates pointer addresses to 48 bits
`src/vm.rs:1283` (`Value::ptr`: `TAG_PTR | (p as u64 & PAYLOAD_MASK)`),
`src/vm.rs:1351` (`as_ptr`).
On platforms with >48-bit virtual addresses (x86-64 LA57 5-level paging,
AArch64 with 52-bit VAs, or any allocator/mmap returning high addresses),
the mask silently truncates the address and every later deref is UB.
The newer `tag_ptr`/`as_ptr_raw` path (`src/value_layout.rs`) uses 32-bit
heap *offsets* and is not affected.
**Fix applied:** new `value_layout::ptr_fits_payload()` guard +
documentation (`src/value_layout.rs`); `ffi/marshal.rs` now fails closed
(returns `nil`) instead of packing a truncated C pointer (F3);
`jit/runtime.rs::alloc_string_value` gained a `debug_assert!`.
**Remaining (vm.rs, excluded):** `Value::ptr` itself should
`debug_assert!(ptr_fits_payload(p as u64))` or be replaced by the offset
encoding; recommend doing this in the vm.rs rewrite.

### F3 — HIGH — FFI packs leaked `CString`/C pointers into truncating payload
`src/ffi/marshal.rs` `cstr_to_value` and `voidptr_to_value` (previously
`Value::ptr(p)` unconditionally).
A truncated pointer would later be passed to `CString::from_raw`
(`free_cstr_value`) or dereferenced by native code — memory corruption.
**Fix applied:** both now route through `ptr_to_value_checked`, which
returns `Value::nil()` when the address exceeds the 48-bit payload
(currently unreachable on supported targets; strictly safer than silent
truncation). Behavior on mainstream x86-64/AArch64 is unchanged.

### F4 — MEDIUM — heap-string reads assume null termination
`src/vm.rs:382` (`CStr::from_ptr(ptr)` — excluded file, noted only),
`src/jit/runtime.rs` (two sites, `resolve_jit_string` and the coercing
resolver).
`CStr::from_ptr` scans until a NUL byte; a foreign-crafted `TAG_PTR` value
(see F1) or heap corruption makes it read out of bounds.
**Fix applied (jit/runtime.rs):** both sites now use the new checked helper
`heap_string_payload()`, which re-verifies the header type tag and bounds
the NUL scan by the payload size recorded in the object header.
**Remaining (vm.rs, excluded):** route `vm.rs:382` through an equivalent
bounded helper during the rewrite.

### F5 — MEDIUM — fat-pointer and signature transmutes rely on de-facto layouts
`src/jit/runtime.rs:387`/`394` (`CbPair` ⇄ `*mut dyn ActorVmCallbacks`),
`src/ffi/marshal.rs:255-316` (raw `*const c_void` transmuted to
`extern "C" fn(...)` of the declared arity).
The trait-object layout transmute is not guaranteed by the language (works on
all Tier-1 targets; documented at the site). The FFI transmutes trust the
registered `Signature` — a wrong signature is instant UB at call time.
**Recommended fix:** keep, but consider `std::ptr::metadata` APIs once
stable; for FFI, this is inherent to dynamic loading — the library allowlist
(`ffi/native.rs::is_lib_allowed`) is the real mitigation.

### F6 — MEDIUM — `unsafe impl Send/Sync` on raw-pointer callback/graph types
`src/runtime/callbacks.rs:807-808` (`BytecodeRuntimeCallbacks`, wraps
`*mut Runtime`), `src/runtime/orca_cycle.rs:131-132` (`ForeignEdge`),
`185` (`ForeignRefNode`), `src/ffi/native.rs:75-78` (`NativeFunction`),
`src/runtime/heap.rs:306` (`ActorHeap`).
All are sound only under the single-scheduler-thread-per-runtime
convention; `BytecodeRuntimeCallbacks: Sync` is the most fragile (a shared
reference allows cross-thread `&mut Runtime` aliasing if the value is ever
shared). These impls previously had **no** SAFETY comment for
`BytecodeRuntimeCallbacks`.
**Fix applied:** SAFETY comments added stating the convention.
**Recommended fix (future):** remove `Sync` for `BytecodeRuntimeCallbacks`
if no consumer requires it, or wrap the runtime pointer in a type that only
yields `&mut Runtime` from `&mut self`.

### F7 — LOW — heap serializer trusts recorded payload sizes
`src/runtime/heap_serialize.rs:342`, `431`, `692`, `706`:
`slice::from_raw_parts(payload, payload_size)` with `slot_count =
payload_size / size_of::<Value>()` — consistent with the allocator
invariants, but any header corruption turns into OOB reads/writes during
state transfer.
**Recommended fix:** `debug_assert!(payload_size % size_of::<Value>() == 0)`
for container tags; bound container slots by the live allocation size.

### F8 — LOW — `ActorHeap::free` silently leaks on corrupt size class
`src/runtime/heap.rs:535`: an out-of-range `size_class` fell through with a
commented "should never happen".
**Fix applied:** `debug_assert!` added so header corruption is caught in
debug/test builds.

### F9 — LOW — `'static` constant pool for JIT helpers
`src/jit/runtime.rs:432-467` (`ConstantsPtr::as_slice` returns
`&'static [Constant]` from a thread-local raw pointer).
Sound by the documented "valid until `clear_jit_constants`" convention;
a missed clear + pool deallocation would dangle. Consider scoping the
helpers to a guard type (`set` returns a `Guard` whose `Drop` clears) so
the lifetime is enforced by RAII.

## Fixes applied in this branch

1. `src/value_layout.rs` — added `ptr_fits_payload()` guard with docs about
   LA57/AArch64-52 truncation + unit test.
2. `src/ffi/marshal.rs` — `cstr_to_value`/`voidptr_to_value` now fail closed
   via `ptr_to_value_checked`; added missing SAFETY note on `CStr::from_ptr`.
3. `src/jit/runtime.rs` — new bounded `heap_string_payload()` helper replaces
   both bare `CStr::from_ptr` derefs; `alloc_string_value` gained a
   `debug_assert!` against 48-bit truncation; missing SAFETY comments added.
4. `src/runtime/callbacks.rs` — SAFETY comments for the
   `BytecodeRuntimeCallbacks` Send/Sync impls.
5. `src/runtime/heap.rs` — `debug_assert!` on out-of-range size class in
   `free()`.

Excluded per instructions (findings noted only): `src/vm.rs` (F1, F2,
F4-vm site), `src/aot/codegen.rs`, `src/mir_wasm.rs`,
`src/integration_tests/mod.rs`.

### F10 — LOW — direct JIT access to VM register storage
`src/vm.rs::VM::try_jit_execute`, `src/backends/mod.rs::JitBackend::tiered_execute_value_regs`,
and `src/jit/mod.rs`.

**Status: MITIGATED (2026-09-20).** `Value` is now explicitly
`#[repr(transparent)]` over one `u64`, allowing the built-in Cranelift ABI to
operate directly on the current frame's 256-register storage without a
copy-in/copy-out transition. The VM passes only a raw pointer; it does not
construct a typed `&mut [u64; 256]` alias over `Value` objects.

The direct pointer is sound only while the frame storage cannot move. Native
direct Nulang calls are re-entrant and can push onto `VM::frames`, potentially
reallocating that vector. `JitSession` therefore marks compiled regions with
native direct calls as re-entrant and executes those regions against the
legacy 256-word snapshot, copying results back after native execution.
Non-reentrant regions use the direct pointer. Alternative `JitBackend`
implementations inherit a copy-based default adapter unless they explicitly
override the direct-register hook.

**Invariant:** `tiered_execute_value_regs` receives a live, uniquely borrowed
pointer to at least 256 `Value` slots for the duration of the call; direct
execution is permitted only for regions that cannot re-enter the VM frame
stack.

## Validation

- `cargo test --no-default-features --lib`: baseline (pre-change) 1714
  passed / 1 failed / 3 ignored; post-change 1715 passed / 1 failed / 3
  ignored (+1 new `value_layout` test, no regressions). The single failure
  is the pre-existing
  `aot::codegen::tests::test_aot_runtime_native_perform_async`, identical
  before and after.


### F1b — CLOSED — raw pointer construction requires provenance
`Value::ptr(*mut u8)` is an `unsafe fn`: safe Rust can no longer manufacture a
pointer-tagged `Value` from an arbitrary/dangling raw pointer. Its safety
contract requires a live pointer with provenance, layout, and lifetime valid
for every runtime consumer that may dereference it. The VM allocation API now
exposes `ActorVmCallbacks::alloc_value`, which allocates storage and constructs
the associated pointer `Value` in one safe operation, so normal heap allocation
does not require callers to assert provenance manually.

Remaining raw-pointer crossings are explicit `unsafe` call sites and fall into
reviewable boundary classes: runtime/GC pointers obtained from the actor/VM
allocator or an existing pointer `Value`; persistence pointers reconstructed
through the live object table; host/WASM bridge storage whose lifetime is held
by the host; and native/FFI pointers governed by the enclosing unsafe ABI
contract. `Value::ptr` also carries a `compile_fail` doctest so a future safe
constructor regression is caught by doctests.
