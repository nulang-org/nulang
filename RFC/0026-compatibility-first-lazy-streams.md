# RFC 0026: Compatibility-first lazy streams

- **Status:** Draft / design contract. Not an implemented language feature.
- **Date:** 2026-10-20
- **Area:** Language, typechecker, VM, host boundaries, conformance
- **Preconditions:** Finish and validate the host stream stack
  [#2065](https://github.com/nulang-org/nulang/pull/2065) →
  [#2067](https://github.com/nulang-org/nulang/pull/2067) →
  [#2072](https://github.com/nulang-org/nulang/pull/2072) →
  [#2073](https://github.com/nulang-org/nulang/pull/2073) →
  [#2074](https://github.com/nulang-org/nulang/pull/2074).
  Resolve independent [#2068](https://github.com/nulang-org/nulang/pull/2068)
  Wasmtime security audit before any production promotion.

## Executive decision

**Do not reserve the unqualified type name `Stream` in the lexer,
parser, or typechecker.** Nulang already supports the legal source
declaration:

```nula
type Stream[T] = Nil | Cons((T, Stream[T]))
```

This is an **eager recursive algebraic data type**, not a demand-driven
stream. The compiler has a specific `Nil`-first sum-type routing
exception for that form in `src/parser.rs`; behavior conformance
`generics_07_recursive_stream_construct_reject.nula` already depends on
it. New behavior fixture `stream_01_user_defined_name_compat.nula`
pins this compatibility contract with an explicit type annotation.
Future standard-library lazy streams must be **namespaced**, or introduced
with a compatibility-preserving import alias, rather than stealing the
user-defined name.

Proposed library-level spelling below uses
`std.stream.Stream[T, E]` and `std.stream.Cursor[T, E]` as **design
notation**. It is NOT verified Nulang source syntax; syntax and qualified
type resolution must first be proved separately. The Rust-host
`PullStream<I>` is also NOT yet this source-language type.

## Core semantics (proposed)

1. **Pull, not push.** One successful call to `next` demands at most
   one additional result from a source. A source cannot eagerly fill an
   unbounded queue or prefetch silently.
2. **First-class terminal result.** The abstract step algebra is
   `Done | Item(value, next_cursor) | Failed(error)`. End-of-stream
   is distinct from source failure and cancellation. No use of `Nil`
   as an implicit error sentinel.
3. **Single owner of a cursor.** Pulling consumes or advances the sole
   cursor authority; stale cursor copies cannot produce duplicate data.
   Prove this using existing linear/affine authority infrastructure
   before claiming source-level `Stream` safety. Until proved, keep
   `Cursor` opaque and host-only.
4. **Effects are explicit.** A pure array-backed source may be pure;
   file/network/subprocess sources require the existing capability and
   effect checks. No ambient IO, shell interpretation, or hidden
   authority inheritance. Each source declares its own effect row.
5. **Error semantics.** `map`, `filter`, `take`, `fold` and
   `collect` preserve the first error. Source and mapping errors are
   not silently discarded. `take(n)` must not demand item `n+1`.
6. **Resource policy.** Any operation materializing records requires
   both item and aggregate byte limits. The preliminary host
   `collect_budgeted` measures caller-declared payloads and is NOT
   an RSS or allocator bound; a language API must label those
   measurements accurately and separately cap raw record bytes.
7. **Cancellation.** Cancellation closes the source exactly once
   and returns an explicit cancelled result. It must not leak a
   spawned reader, OS process, open file or outstanding mailbox
   subscription. Cancellation is NOT equivalent to a source error or
   normal EOF.
8. **Durable determinism.** Streams over live IO are transient by
   default. A workflow must not replay a stream's live reads. Durable
   streams require explicit journaled input receipts and stable
   resume tokens; a cursor cannot be serialized as a raw process
   pointer.
9. **Multi-consumer semantics.** A single cursor is not broadcast.
   Sharing, fan-out and replay require explicitly bounded queues and
   separate backpressure / lifecycle contracts.
10. **Canonicalization is separate.** Duplicate-key rejection
    in JSONL is required before `serde_json::Value` materialization
    for ambiguity-sensitive input, but does not make input
    cryptographically canonical or enforce Unicode normalization.

## Phased delivery

### Phase 0: host proofs and security (blocking)

Complete the five stacked host-side PRs, including focused tests,
exact-head full CI, formatting, and audit against the patched Wasmtime
dependency graph. Do not infer `cargo fmt` success from passing
`cargo check`, and do not infer production heap bounds from
payload-byte limits. Keep the independent process-execution PR #2063
isolated from the stream-type rollout.

### Phase 1: compatibility and syntax proof

- Preserve the current user-defined recursive `Stream[T]`
  declaration and generic type application.
- Choose one namespaced source-level type spelling after checking
  qualified-type resolution, generic arity, and module imports.
- Add parser/formatter/LSP golden cases for qualified type arguments,
  explicit error type parameters, and imports. Negative cases must
  fail on unknown stream types, wrong arity, and accidental
  shadowing only when a user explicitly opts into a conflicting name.
- **Do not add a global `Type::Stream` variant** until it has a
  well-defined lowering contract and a compatibility migration plan.

### Phase 2: pure demand-driven reference

- Define an opaque, library-owned cursor and explicit
  `Done | Item | Failed` step result without ambient side effects.
- Implement finite array/list-backed `from_items`, `map`,
  `filter`, `take`, and `fold`, initially in one reference
  interpreter path behind an experimental flag.
- Prove zero prefetch, error order, no duplicate pulls, bounded
  terminal consumption, and explicit termination.
- Add negative typechecker cases for cursor reuse, source/consumer
  element mismatch, and effect amplification.

### Phase 3: backend and ownership parity

Before public availability, run identical semantic fixtures in the
parser/typechecker, interpreter, bytecode VM, AOT, and WASM backend
where supported. Mark unsupported backends as **rejected at compile or
deployment**, never silently substituting a stub.

Carry cancellation, ownership and drop/cleanup behavior across all
implementations. Instrument per-pull allocations, read amplification,
in-flight demand, and cleanup events.

### Phase 4: authoritative effects and durability

Introduce explicitly authorized file, network and (only through the
exact-allowlist process boundary) subprocess sources. Demand,
cancellation, timeout and byte budgets are enforcement requirements.

Separate transient sources from durable replay-safe streams. Persist
receipts for consumed inputs before advancing durable cursors; prove
at-most-once external side effects and recovery behavior using
deterministic fault injection.

### Phase 5: optional high-level syntax

Only after the above proofs, consider pipeline/comprehension syntax
as sugar over the typed library API. All syntax must lower to the same
typed effects and authority checks. Operators cannot bypass
bounded collections or turn infinite sources into eager arrays.

## Required conformance matrix

| Layer | Positive contract | Negative contract |
|---|---|---|
| Existing grammar | Local `type Stream[T]` remains valid | No implicit reservation of `Stream` |
| Types | `T` and `E` propagate through transforms | Wrong generic arity, invalid element and stale cursor |
| Demand | Each pull fetches one step | `take(n)` never reads item `n+1` |
| Errors | First failure is preserved | No default swallowing or partial successful collect |
| Budgets | Exact item/byte boundary succeeds | First excess item errors, arithmetic overflow fails closed |
| Ownership | Close/drop once after EOF | Cursor reuse, duplicate close or stale resume denied |
| IO authority | Explicit capability and effect | No ambient file/net/process access |
| Backends | Identical outputs and errors | Unsupported backend fails visibly, not a no-op |
| Durable | Receipt-bound deterministic replay | Live IO cannot be transparently replayed |
| Cancellation | Source cleanup and terminal cancelled result | No orphan processes, threads or subscriptions |

## Merge and compatibility gates

**RFC and compatibility regression may land independently** after
the new conformance fixture is executed. Runtime implementation cannot
land until host stack and Wasmtime audit are green on their exact
heads. Phase 1 changes must not regress the existing
`generics_07` / `typeclass_08` Stream ADT cases. Phase 2 and beyond
require interpreter+VM parity evidence and explicit negative
capability and ownership tests, not just documentation and host Rust
unit tests.

## Open design decisions

- Precise namespace/import spelling and generic error-parameter syntax
  consistent with current Nulang parser.
- Whether cursor authority can use the existing linear-isolation
  capability directly or needs an opaque runtime handle.
- Exact cancellation error shape, lifecycle hook, and whether `close`
  is exposed.
- How item and raw/encoded byte accounting compose without counting
  heap allocator overhead as a safety guarantee.
- Backend coverage policy for WASM, browser, remote workers and durable
  actors.

Decision owners should resolve these with executable conformance cases
before declaring the language-level API implemented.
