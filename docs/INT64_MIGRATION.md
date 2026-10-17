# Int64 migration contract (pre-adoption, in progress)

**Status:** Phase 1 kernel implemented; not yet a change to Nulang's source-language `Int` or serialized ABI.

Nulang currently stores a signed 48-bit integer in an 8-byte tagged `Value`.
The runtime accepts `i64` at many input boundaries but `Value::int` masks
to 48 bits; some operations wrap, while checked negation can fail. This
inconsistency is a correctness limitation for a language intended to host
distributed databases, durable actors and geospatial applications.

## Target semantics

- `Int` denotes a **signed 64-bit integer**, range `i64::MIN..=i64::MAX`.
- `+`, `-`, `*`, unary negation, division, remainder and integer
  exponentiation return the precise mathematical result when it fits in that
  range. Overflow is a typed arithmetic error. There is no release/debug
  change in behavior and no implicit float coercion.
- Division and remainder by zero fail explicitly. `i64::MIN / -1` and
  `i64::MIN % -1` are overflow errors rather than traps or wraps.
- `0 ** 0 == 1`. Negative integer exponents produce an explicit domain
  error, not `nil` or a floating-point conversion.
- Bitwise and shift operations need separate specification. Shifts must
  reject invalid shift counts or use explicitly named masking operations.
- Users who require modulo arithmetic should receive **explicitly named
  wrapping operations**; don't overload ordinary arithmetic with wrapping.
- Numeric literal parsing, constant folding, type inference, FFI conversion,
  native SSA, VM, AOT, WASM and persistence must obey one specification.

## Value layout target

Keep the existing 8-byte tagged `Value` representation for common values.
Integers in `[-2^47, 2^47 - 1]` remain inline. Wider `i64` values
require a representation that preserves the entire value:

- Typed native/JIT code should carry proven `i64` values directly, boxing
  only at generic-value, capture, message or persistence boundaries.
- Dynamically boxed integers require an owned wide-int heap object with
  normal ORCA reference semantics and precisely specified provenance.
- Cross-actor messages must copy or retain wide integers as data, never
  serialize raw process-local pointers.
- Persisted `Int` values and compiler constants must encode signed 64-bit
  numbers independent of the in-memory tag layout.
- The existing v1 `.nbc`, NUL0 and value-layout artifacts must be
  versioned/migrated or explicitly rejected, not silently reinterpreted.

## Phase 1b — ORCA-owned, scoped wide-int allocation

Stacked follow-up PR after the pure Int64 kernel:

- A dedicated `TypeTag::WideInt = 9` is an **in-memory** ORCA scalar with an
  8-byte aligned `i64` payload. It is not a publicly serialized TypeTag.
- `Actor::lease_wide_int` uses `OrcaGc::alloc_object` and returns an
  exclusive lifetime-scoped `WideIntLease`. The scalar remains readable
  without truncation; dropping the lease releases exactly one local ORCA ref.
- The lease intentionally exposes **no raw pointer and no VM `Value`**.
  It cannot outlive the actor, enter actor state, cross a mailbox, be
  captured into a durable frame, or cross FFI. This conservative boundary
  avoids accidentally making an unsupported transport promise.
- `serialize_continuation` refuses a reachable wide-int heap object in
  NLCS v1. The v1 decoder continues to reject discriminant 9 as unknown,
  preserving existing format semantics instead of reading incompatible data.
- This is an ownership proof for scoped scalars, **not yet** the eventual
  boxed generic `Value` representation. A future ref-counted wide Value
  must integrate stack-slot barriers, GC root scans, containers, actor sends,
  snapshots, and backend helper ABI before source-level `Int` is widened.

## Phase 1c — generic tagged values with rooted register barriers

The follow-up branch uses `RootedIntRegisters` to model VM register ownership
while preserving the existing single-word `Value` layout. The bank
exclusively borrows an actor's ORCA heap and GC. Small signed integers are
`TAG_INT` values; full-width values are `TAG_PTR` values pointing to
ORCA-managed `TypeTag::WideInt` payloads. A pointer range check prevents
the legacy 48-bit pointer codec from silently truncating high addresses.

Each register roots its own object. `copy_into` increments the local
reference count before replacing the destination; `move_into` transfers
ownership; overwrite, clear and bank drop decrement exactly once. Checked
arithmetic validates a new `i64` before mutating the destination.
Overflow, bounds errors and allocation failure retain the previous root.

**Still gated:** The public bytecode VM does not use this register bank.
Raw tagged bits remain private. Cross-actor messages, async suspension,
containers, FFI and NLCS v1 snapshots cannot carry these wide roots. The
source language's `Int` remains 48-bit until backend and durability
parity are proven.

## Current code in this PR## Phase 1d — bounded interoperability with production VM Frame

`RootedIntFrame` now provides a fail-closed numeric adapter to
`crate::vm::Frame`, the *actual* production VM activation frame with
256 raw `Value` registers. The existing Frame is publicly cloneable,
and continuations/suspended executions duplicate or move its raw registers
without retaining ORCA roots. Consequently, **wide pointer values must not
be placed directly into a legacy Frame** before its full ownership redesign.

The adapter imports legacy frames only when every register is either nil
or a signed inline integer, and rejects closure environments or spilled
slots. It explicitly refuses exports containing full-width integers that
do not fit the old 48-bit payload. No raw tagged pointer escapes.

The VM now exposes `VM::install_numeric_rooted_frame`, which checks the entire rooted frame and installs it using the existing `set_current_frame` only if every value is representable by the legacy inline ABI. Failed wide-value exports leave the VM's previous frame unchanged. This is a real production VM entrypoint, but intentionally not a wide-integer execution path.

A semantic, pointer-free `FrameInt64Snapshot` can capture signed 64-bit
values and restore them as *new* actor-owned ORCA roots. This is only an
in-process staging snapshot, **not** a versioned persistence, wire or
bytecode format. The actor heap is exclusively borrowed while rooted
frames exist.

The remaining production-VM integration requires explicit retain/release
barriers at every Frame clone/copy/drop, register/spill overwrite, closure
capture, JIT/AOT ABI boundary, actor message and persistence/replay edge.
Until those are implemented and differential tests pass, the public source
language remains 48-bit and existing Frame behavior is unchanged.



`src/int64_semantics.rs` implements the target **pure checked arithmetic
kernel** and an **opaque staging** `Int64Storage` that distinguishes inline
integers from full-width values without dropping high bits.

`value_layout::try_tag_int` and `Value::try_int` are checked opt-in
constructors. The original `Value::int`, `tag_int` and v1 arithmetic are
unchanged for compatibility. In particular:

**The source-language `Int` is still 48-bit on this branch.** These new
functions prove only the arithmetic rule and the encoding boundary; they
are not evidence that interpreter, JIT, AOT or WASM already supports
full-width `Int`.

## Remaining implementation gates (must not be skipped)

1. **Typed semantic normalization:** audit and specify opcodes, scalar
   arithmetic, constants, overflow and div/rem behavior in one compiler-owned
   rule set. Resolve the current inconsistencies before changing a backend.
2. **Wide generic value ownership:** add an ORCA-owned wide-int representation;
   validate aliasing, drop, copy, allocation failure, suspension snapshots and
   actor messages. Avoid a global side table keyed by value bits.
3. **Bytecode interpreter + CoreVM:** lift all relevant integer operations,
   equality, conversions, literal compilation and runtime effect marshalling.
4. **JIT/native/WASM parity:** lower native typed `i64` and shared boxed
   helpers to equivalent behavior. Add boundary-sensitive differential fuzzing.
5. **Persisted identity:** version or migrate .nbc constants, runtime values,
   snapshots, journals, wire values, Python/C FFI and cloud effect receipts.
6. **End-to-end conformance:** run checked arithmetic, overflow, reference
   retention, crash/recovery and cross-node messaging tests on all targets.

### Required verification

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo check --all-targets --no-default-features
cargo test --test int64_semantics_kernel
cargo test --no-default-features --test int64_semantics_kernel
cargo test --lib
```

Before a semantic migration is promoted, additionally run interpreted/JIT/AOT/
WASM differential tests and the relevant persisted-format and durable-replay
conformance gates on its exact head. Green kernel tests by themselves do not
certify the language transition.

## Relationship to governance

This is an Incubating-Core improvement under RFC 0032. Incubating means the
language may evolve; it does not waive versioning of already emitted artifacts
or correctness checks. Do not combine a wide-int change with an unrelated
protocol-format break in the same commit.
