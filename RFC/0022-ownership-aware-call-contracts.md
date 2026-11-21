# RFC 0022: Ownership-Aware Call Contracts

- **Status:** Draft
- **Tier:** Experimental
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-09-17
- **Resolved:** TBD
- **Language-version at effect:** N/A until accepted
- **Supersedes:** none
- **Superseded by:** none

## Summary

Define how Nulang's compile-time capability/linearity guarantees map onto
runtime reference-count ownership at function-call boundaries.

Today function calls are intentionally ownership-neutral at runtime:

1. MIR call arguments are copied into caller staging registers;
2. the VM copies staging registers into a new frame;
3. the callee prologue copies them again into parameter locals;
4. no retain/release occurs for those copies;
5. parameter locals are excluded from Drop planning;
6. capabilities are erased before runtime.

That convention is safe only because the caller remains the counted owner for
ordinary calls. It also means a source-level exactly-once parameter cannot yet
act as a Nim-style sink merely because it is declared `lineariso` or
`linear`.

This RFC introduces an ownership-call model in two stages:

- **Phase A — whole-module inferred ownership ABI:** an optimization for
  internal, statically known direct calls. No new source syntax and no bytecode
  format change.
- **Phase B — first-class ownership signatures:** a stable type-level contract
  for exported/higher-order functions, after the direct-call model has been
  proven.

The load-bearing rule is:

> Capability semantics and runtime ownership are related but not identical.
> The compiler may transfer a counted reference across a call only when both
> the caller and callee agree on the same ownership contract.

## Motivation

PRs #384, #385, #399, and #400 establish the local ownership pipeline:

- `consume x` is a real runtime move-out;
- parameter capabilities survive HIR -> MIR;
- MIR records explicit ownership-transfer edges;
- Drop planning understands transfer rather than treating every `Load` as
  alias creation;
- conservative last-use transfer inference can move ownership through compiler
  temporaries.

The remaining boundary is `RValue::Call`.

### Current call path

The bytecode backend currently behaves conceptually as:

```text
caller local a
    |
    | Move (no retain)
    v
caller r0 staging
    |
    | VM frame copy (no retain)
    v
callee r0 incoming
    |
    | Move (no retain)
    v
callee parameter local
```

All four registers can temporarily contain identical raw pointer bits, but only
the caller's original ownership slot is treated as counted. This is why the
Drop planner excludes parameters and why call arguments are classified as
uncounted `Copy` uses.

Blindly changing `lineariso` parameters into owning parameters would create
one of two bugs:

- if the caller keeps ownership and the callee drops the parameter:
  **double release / use-after-free**;
- if the caller gives up ownership but the callee remains excluded from Drop
  planning: **leak**.

Return values, forwarding calls, indirect functions, sends, and captures make
the problem transitive.

## Existing semantic facts

Nulang already distinguishes several source guarantees that must remain
separate:

- `lineariso`: unique, mutable, exactly-once binding use;
- `linear`: immutable/sendable, exactly-once binding use;
- `iso`: unique, but ordinary function calls do **not** consume it;
- `val`, `ref`, `box`, `tag`, `trn`: non-linear call semantics.

The capability analyzer currently treats an ordinary application of `iso` as
a borrow, while linear bindings have an exactly-once obligation.

Therefore Phase A may use `lineariso` / `linear` as evidence that a binding
can participate in a transfer, but it must not reinterpret ordinary `iso`
calls as consuming calls.

## Design

### 1. MIR ownership modes

Add compiler-internal call ownership descriptors:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamOwnership {
    Borrowed,
    Owned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnOwnership {
    BorrowedOrImmediate,
    Owned,
}

pub struct OwnershipSignature {
    pub params: Vec<ParamOwnership>,
    pub ret: ReturnOwnership,
}
```

These are MIR/compiler metadata in Phase A. They are **not** encoded into the
frozen bytecode format and do not alter `OpCode::Call`.

### 2. Phase A is whole-module and direct-call only

A function parameter may be promoted to `Owned` only if the compiler can
prove all call sites and all uses of the function.

Eligibility requires:

1. the function body and every relevant call site are in the same compilation
   unit;
2. every call is a statically resolved `FuncRef::Index`;
3. the function does not escape as a first-class value or closure target under
   a type that erases ownership information;
4. every promoted parameter has an exactly-once source capability
   (`lineariso` or `linear`) or an explicit ownership-transfer expression;
5. every call supplies a value whose ownership can be transferred;
6. control flow cannot invoke the function through an unanalysed dynamic path.

If any condition fails, the parameter remains `Borrowed`.

This makes Phase A an optimization, not a source-semantic compatibility break.

### 3. No partial agreement

Ownership promotion is per parameter, but agreement is module-wide.

For a given function parameter, the compiler must never generate:

```text
call site A -> Owned
call site B -> Borrowed
callee param -> Owned
```

because one callee entrypoint cannot safely drop a parameter that some callers
still own.

The promotion decision is therefore:

```text
Owned(param_i) =
    callee_eligible
    AND all_call_sites_transferable(param_i)
    AND callee_flow_safe(param_i)
```

Otherwise it is Borrowed everywhere.

### 4. Caller-side transfer

For an `Owned` argument, bytecode generation keeps the existing staging
sequence and invalidates the caller's ownership slot only after the value has
been staged:

```text
Move caller_local -> r0
ConstNil caller_local
Call ...
```

The staging register contains the raw value needed by the VM frame copy.
Clearing the source does not release it; it transfers the counted ownership
slot.

This is the same semantic operation already represented by
`OwnershipTransfer`.

The compiler must not emit `Drop caller_local` at the transfer.

### 5. Callee parameter ownership

An `Owned` parameter is a counted owner at function entry even though its MIR
value originates outside the function.

Drop planning therefore seeds owned parameters as entry ownership definitions
instead of excluding them.

A borrowed parameter remains excluded exactly as today.

Conceptually:

```text
Borrowed parameter:
    caller keeps counted slot
    callee may read/retain
    callee never releases parameter slot

Owned parameter:
    caller clears counted slot
    callee receives counted slot
    callee must release or transfer it exactly once
```

### 6. Incoming staging registers are aliases, not extra owners

The VM and callee prologue may continue to copy the raw value:

```text
caller staging -> new frame r0 -> parameter local
```

Those registers do not each gain reference counts.

After the prologue copies an owned incoming value into its parameter local, the
backend may clear the incoming staging register for debugger clarity and to
reduce accidental future coupling, but this is not required for reference
count correctness.

### 7. Owned-parameter dataflow

An owned parameter must end in one of these states on every path:

- **released** after its last non-transferring use;
- **retained into another owner** and then released locally;
- **transferred** to another local/owned call/send boundary;
- **returned as owned**;
- **diverged** (panic/non-returning path with cleanup semantics defined).

The ownership-flow analysis from #399/#400 should be reused. A separate
call-only ownership system is rejected.

### 8. Forwarding into another owned call

When owned parameter `x` is forwarded:

```nula
fn outer(lineariso x: T) {
    inner(x)
}
```

and `inner`'s corresponding parameter is also promoted to `Owned`, the call
use of `x` is an ownership transfer rather than an uncounted copy.

The caller frame for `outer` clears its local after staging; `inner` becomes
the owner.

This allows ownership to move through call chains without retain/release churn.

### 9. Calls to borrowed parameters remain escape points

If an owned local is passed to a borrowed parameter, the owner must stay alive
for the duration required by that borrow.

Phase A should remain conservative:

- the use is not a transfer;
- the owner is not reclaimed before the call;
- if borrow lifetime cannot be bounded to the synchronous call, promotion is
  rejected.

Ordinary direct synchronous functions can eventually use interprocedural escape
summaries to prove non-escaping borrows.

### 10. Retaining operations

Operations that create a counted reference, such as storing a value into an
owning aggregate, do not automatically transfer the source.

Example:

```text
owned param x
tuple = Tuple(x)   // tuple retains x
Drop x             // local ownership released
                 // tuple remains owner
```

This remains compatible with the existing Drop planner's `Retaining` use
classification.

### 11. Return ownership is a separate contract

A function may transfer ownership to its caller.

This cannot be inferred merely from the source parameter capability because a
function can:

- return a fresh aggregate;
- return an owned parameter;
- return a shared/borrowed value;
- return an immediate scalar.

Phase A computes a `ReturnOwnership` summary from MIR ownership flow.

For an owned return:

1. the callee does not drop the returned owner;
2. the return register carries the existing counted slot;
3. the caller's call-result local becomes an owning definition.

The bytecode `Ret` / `RetVal` instructions remain unchanged.

### 12. Error/unwind cleanup

Ownership transfer at call entry creates a hard requirement: if execution
abandons a callee frame due to a runtime error, every live owned parameter and
owned local in that frame must still be released exactly once.

Before Phase A can be enabled for general code, the implementation must audit
all non-local exits:

- runtime error return from VM stepping;
- explicit panic;
- effect-handler unwinding;
- cancellation/suspension paths;
- actor failure/restart;
- native/AOT error bridges.

If frame unwinding does not currently run Drop cleanup, ownership promotion must
remain disabled on paths that can escape that cleanup discipline.

This is a correctness gate, not an optimization-quality issue.

### 13. Tail calls

Tail calls need special handling because the current `TailCall` reuses a
frame.

An owned parameter forwarded through a tail call can be transferred without a
retain/release only if:

- every live owner in the outgoing frame is either released or forwarded;
- the target's ownership signature is known;
- overwritten registers are cleaned according to ownership state.

Phase A may simply exclude tail-call sites until this is implemented.

### 14. First-class functions are Phase B

Current `Type::Function` stores:

- aggregate parameter type;
- return type;
- effect row;
- function-reference capability.

It does **not** store per-parameter ownership/capability modes.

Therefore an ownership-taking function cannot yet safely expose that contract
through an arbitrary first-class function value.

Phase A solves this by requiring whole-module direct-call proof.

Phase B must make ownership part of callable type identity, for example through
a stable ownership signature attached to function types or a dedicated callable
descriptor.

This requires a separate compatibility review because canonical type hashing,
typeclass matching, exports, tooling, and higher-order inference are affected.

### 15. Closures

Closure calls are borrowed-only in Phase A unless the compiler can prove the
closure target and its complete ownership signature.

A local closure that is fully inlined before ownership analysis naturally
reduces to ordinary MIR and can benefit from #400 without a call ABI change.

This is another reason to run local closure inlining before ownership inference.

### 16. Exported functions / FFI

Public/exported functions and FFI boundaries remain borrowed under Phase A.

They may not receive an implicit ownership ABI that external callers cannot
observe.

A future stable ABI must specify ownership explicitly in generated headers/WIT
metadata and version that contract independently of internal optimization.

### 17. Actor send/ask are not ordinary calls

Actor messaging already has separate sendability/transfer semantics and may
cross heap/node boundaries.

This RFC does not reinterpret `send` or `ask` as ownership-aware function
calls.

Their ownership protocol should consume the same MIR ownership-flow facts but
remain a separate boundary policy.

### 18. Interaction with `consume`

Explicit `consume` remains the strongest local signal.

If an argument is already represented by an explicit ownership transfer into a
compiler temporary and the callee parameter is promoted to `Owned`, the
compiler may coalesce the transfer chain:

```text
x -> consume temp -> staging -> callee param
```

into one logical ownership path without extra retain/release operations.

If the callee is borrowed, the temporary remains the owner in the caller and
must survive the call.

### 19. Static capability validation

Phase A must not rely solely on MIR shape.

The compiler must retain enough semantic metadata to prove that an owned call
does not violate source capability rules.

At minimum:

- `lineariso` and `linear` bindings are consumable once;
- ordinary `iso` calls remain borrowed unless an explicit consuming operation
  is present;
- `ref` / `box` / `val` are not silently converted into exclusive
  ownership;
- capability errors are reported at the source call, not as an internal MIR
  failure.

### 20. Optimization summary

The compiler should eventually expose ownership-call decisions in MIR/debug
output:

```text
fn process
  param data: owned (all 3 direct calls transfer)
  param config: borrowed
  return: owned

call main -> process
  data: transfer local %17
  config: borrow local %22
```

This is diagnostic tooling, not runtime metadata.

### 21. Proof obligations / tests

Before Phase A is enabled by default, tests must cover:

1. owned parameter used and dropped;
2. owned parameter forwarded to another owned call;
3. owned parameter retained into tuple/array/record;
4. owned parameter returned;
5. branch where ownership transfers on one path and drops on another;
6. recursive direct calls;
7. rejected mixed borrowed/owned call sites;
8. named caller source is cleared after owned call;
9. indirect/closure call remains borrowed;
10. panic/error path does not leak or double-drop;
11. spill-register owned parameters;
12. >1 nested call frames;
13. interpreter / WASM / AOT parity;
14. actor behavior boundaries are unaffected;
15. differential stress test of retain/drop counts.

### 22. Implementation order

Recommended implementation sequence:

1. **Frame cleanup audit.**
   Prove or implement release of live owners on all non-local exits.
2. **Ownership summaries.**
   Add MIR-only param/return ownership metadata.
3. **Direct-call graph eligibility.**
   Identify functions whose complete call graph is statically visible.
4. **Owned parameter Drop planning.**
   Seed selected parameters as entry owners.
5. **Caller transfer insertion.**
   Stage first, then clear transferred caller locals.
6. **Owned return flow.**
   Transfer ownership into call-result locals.
7. **Forwarding fixed point.**
   Propagate ownership across chains of eligible direct calls.
8. **Backend parity tests.**
   Bytecode, WASM, AOT.
9. **Phase B RFC.**
   Only then define ownership in first-class/exported function types.

## Tier Classification

Experimental.

Phase A is an optimization over existing source semantics and should not modify
the frozen bytecode opcode layout, VM value representation, or wire protocol.

Phase B would affect stable type semantics and requires a separate acceptance
decision before ownership signatures become source-visible or part of exported
ABI identity.

## Backwards Compatibility

Phase A must be observationally equivalent for valid programs:

- same values/results;
- same effect behavior;
- same capability diagnostics;
- earlier or fewer retains/releases are not source-observable;
- debugger-visible moved state may only change where the source language
  already considers the binding consumed.

If correctness cannot be proven for a call site, it remains borrowed.

## Alternatives Considered

### Treat every `lineariso` parameter as an owned runtime parameter

Rejected. First-class/indirect callers currently carry no ownership signature,
and parameter locals are not runtime owners today. This would create
double-release or leak bugs.

### Add a `sink` keyword immediately

Rejected. A keyword does not solve the ABI, return, unwind, higher-order, or
Drop-planning problems. The compiler needs the ownership model first.

### Retain every argument on call and release every parameter

Rejected as the baseline. It is simpler but adds RC traffic to every call,
including pure borrows, and undermines the optimization goal.

It could serve as a temporary correctness oracle in tests.

### Encode ownership in new Call opcodes

Rejected for Phase A. Existing raw copy instructions are sufficient; compiler
metadata can place clears/drops without changing the frozen bytecode format.

### Make `iso` calls consuming

Rejected. Existing capability semantics explicitly allow repeated ordinary
calls with `iso`; changing that would be a language break.

## Open Questions

1. Should `linear` and `lineariso` both imply eligibility for Phase-A owned
   parameters, or should mutable/immutable ownership use separate summaries?
2. What is the canonical ownership rule for values returned from a function
   whose type is structurally unqualified but whose MIR source is owned?
3. Should error unwinding gain a generic frame ownership bitmap, or should
   bytecode codegen synthesize cleanup blocks before every escaping error?
4. Can recursive strongly-connected call components be promoted atomically, or
   should Phase A initially exclude recursion?
5. What representation should Phase B use so ownership survives higher-order
   type inference without conflating reference capability with transfer mode?
6. Should a future explicit call-site operation use existing `consume` only,
   or is a separate ownership-call annotation useful once semantics are proven?

## Resolution

Pending.
