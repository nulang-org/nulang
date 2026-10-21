# Experimental MIR access analysis (Hylo-inspired)

**Status:** compiler-only prototype; no new source syntax, runtime behavior, or optimization claims.
**Source:** `src/mir_access.rs`
**Focused tests:** `cargo test --locked --lib mir_access::tests -- --nocapture`
**Full tests:** `cargo nextest run --locked` (when `cargo-nextest` is installed).

## Existing semantics remain authoritative

Nulang's reference capabilities (`iso`, `lineariso`, `ref`, `val`, etc.) and effect checker remain the authority for current programs. The existing linear-consumption and actor-sendability checks are **not** replaced.

This prototype separates three *future* categories: `Read`, `Modify`, and `Consume`. It introduces neither `inout`/`sink` source syntax nor inferred ownership transfer. In particular, MIR `RValue::Load` can mean a copy/reference use and is **not** proof of consumption. The MIR summary deliberately records no implicit consume.

## Read-only MIR facts

`summarize_function(&mir::Function)` yields access sites with:

- exact register definitions (which are not heap writes);
- record-field or array access projections;
- aggregate mutations;
- conservative operation boundaries (call, effect, possible suspension, FFI, actor interaction, handler, durable operation, etc.).

MIR array indices are locals, not proven constants, so their projections use `Index(None)`. A later constant-propagation pass may establish `Index(Some(n))`, but this module does not infer it.

Different MIR locals may alias. `may_overlap` therefore treats **different roots as potentially overlapping** unless a future provenance-aware analysis supplies a proof. Two named fields or two known-distinct indices under the *same proven root* can be recognized as disjoint slots. Projection disjointness does **not** establish that referenced objects inside those slots are disjoint.

## Explicit scoped checker

`validate_scoped_access(events)` tests a proposed sequence of frontend-proven `Begin`, `End`, `Consume`, `Reinitialize`, and `Boundary` events.

It rejects:

1. overlapping `Modify` access while another overlapping read or modify is live;
2. consume while any potentially overlapping access is live;
3. use after consumption until a **whole-root** reinitialization is established;
4. unmatched/unclosed scopes;
5. live borrows across unknown calls, effects, possible suspension, FFI, handler boundaries, durable effects, or actor boundaries.

No ordinary MIR instruction is automatically converted into `Begin`, `End`, or `Consume`. Doing so would invent provenance that current MIR does not encode. The checker is **not invoked from production HIR/MIR lowering** yet, and its presence does not enforce new guarantees on user programs.

## Promotion gates

Before wiring this checker into source typing/lowering, require:

- source origin and alias provenance, including scope identity and the lifetime of nested projections;
- proof of reinitialization and path-sensitive move tracking at CFG merges/loops;
- explicit contract for effect handlers, multi-shot continuations, async suspension, actor sends, FFI, and durable replay;
- diagnostic spans pointing to the conflicting source accesses;
- accept/reject conformance fixtures and VM/JIT/WASM/AOT differential tests on supported paths;
- allocation, retain/release, and copy measurements **before** any ownership-based optimization.

In particular, this pass must **never** be used yet to eliminate a retain/release or claim that two MIR registers own distinct values. Its value is a small testable vocabulary and conservative foundation for subsequent RFC-backed work.
