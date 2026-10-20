# ORCA local-cycle root inventory (partial implementation)

Status: **inspection-only root visitors; local-cycle reclamation must remain disabled**. See [issue #1259](https://github.com/nulang-org/nulang/issues/1259).

## Why the root inventory is required

Nulang's `Value` registers can alias a heap object without incrementing the
object's ORCA local reference count. Therefore a local strongly connected
component is **not** unreachable just because its incoming counted edges
account for every `ref_count`. A live register or suspended continuation
may still reference it. Any local-cycle collector must use an authoritative
root snapshot at a scheduler safepoint and confirm object provenance.

## Implemented read-only inspection

| Source | Visitor | Status |
| --- | --- | --- |
| Active/cached VM frames: all 256 registers, spills, closure environments | `VM::visit_execution_values` | Read-only |
| Active handler single-shot register snapshots | `VM::visit_execution_values` | Read-only |
| Nested captured effect continuations, their frames and handler snapshots | `VM::visit_execution_values` | Read-only; iterative traversal |
| Suspended workflow frames/handlers | `SuspendedVmState::visit_execution_values` | Read-only |
| Actor `state_data` | `Actor::visit_stored_values` | Read-only |
| Actor `event_log` payload values and `query_handlers` | `Actor::visit_stored_values` | Read-only |
| Actor `suspended_execution.vm_state` | `Actor::visit_stored_values` | Read-only |
| Scheduler-local mailbox queue, three selective-receive staging buffers, active candidate | `Mailbox::visit_scheduler_owned_values` (also called by `Actor::visit_stored_values`) | Read-only; **excludes concurrent queues** |
| Exclusive mailbox inspection, including SegQueue system/normal lanes | `Mailbox::visit_all_queued_values_exclusive`, `Actor::visit_all_stored_values_exclusive` | **Requires exclusive actor/mailbox ownership**; copies and restores queues; not a concurrent producer root barrier |
| Messages retained pending bytecode fetch | `Runtime::visit_pending_fetched_values` | Read-only; **excludes active channels** |
| An individually owned cross-shard envelope (two delivery variants) | `CrossShardMsg::visit_payload_values` | Read-only; **does not enumerate channels** |
| Exact live allocation identity in a single actor heap | `Actor::is_live_heap_payload_value` | Current address-provenance check only; **not generation-aware** |
| Unsharded non-executing runtime diagnostic view | `Runtime::orca_diagnostic_roots_if_not_executing` | Actor, mailbox, VM and pending-fetch Values; **not complete**, never used for reclamation |

These APIs emit **Values**, not verified heap addresses. A collector must
check each pointer's allocation provenance and owner before dereferencing it.
The visitors scan conservatively, including stale frame slots; extra roots
may delay collection, but missing roots could free live data.

## Blocking gaps: do not promote this to a tracing collector

1. **Concurrent mailbox admission:** normal and system queues use
   `crossbeam::SegQueue`. The new exclusive snapshot can inspect both by
   temporarily draining/restoring them while holding a unique `&mut Mailbox`,
   and the scheduler-local/staged visitor still works without queue mutation.
   This is **not** an admission/root publication barrier for concurrent
   producers. Do not invoke it during a send, from another thread, or in the
   presence of arbitrary unsafe aliases. Before collection relies on it,
   prove a scheduler safepoint that excludes producers and preserves pending
   cross-shard/FFI/JIT and foreign-held roots. Running it each turn would
   allocate/copy messages; benchmark overhead before ever enabling it.
2. **In-flight values:** individually owned `CrossShardMsg` envelopes and
   `pending_fetched_messages` have read-only payload visitors, but shard
   channel queues, send/receive handoffs, forwarded deliveries and other
   pending operations are not covered by a complete lifetime barrier. A
   single envelope inspection does **not** prove delivery-channel safety.
3. **Native and JIT execution:** interpreter staging registers, runtime callback
   temporary Values, detached JIT register arrays, native/FFI handles, and
   top-level `main_heap` roots are not necessarily represented by the
   persistent `VM.frames` vector at a collection point.
4. **Foreign holds and retired heaps:** `held_foreign_refs` and in-flight
   ORCA credits can keep objects alive even after an owner exits. The current
   central cycle graph must not substitute for lifetime-accurate holds.
5. **Stable object identity:** the new `Actor::is_live_heap_payload_value`
   checks whether a candidate is an exact payload pointer found by scanning
   the actor's live allocations, without dereferencing that candidate. It
   rejects interior/foreign/freed pointers at inspection time, but address
   reuse can turn a stale pointer into a *different* live object. Allocation
   generation or another durable identity is still required before collection.
6. **Concurrency and safepoints:** define an actor-local collection barrier,
   avoid mutating any root during traversal, and account for suspended
   workflows that retain pointers between turns.

**Fail-closed rule:** until all these paths are covered and asserted in
integration tests, never use these visitor results to reclaim any object.

## Runtime diagnostic audit, not a collector safepoint

`Runtime::orca_diagnostic_roots_if_not_executing(actor_id)` is a
`pub(crate)` inspection helper intended for tests and debugging. It refuses
unknown actors, sharded runtimes or attached cross-shard channels, an active
VM/scheduler actor, deferred receive wakes, or a running actor. For a permitted
unsharded runtime it gathers Values from the shared VM, pending bytecode
fetches, the target actor's state/suspensions and its exclusive mailbox
snapshot. It does not mutate ORCA refcounts or free any object.

A non-executing scheduler **is not** a global safepoint: the actor can have
queued work, a JIT/native handle may be retained elsewhere, and runtime
timers/foreign credits/channel lifetimes are not covered. The method does not
report `safe_to_collect` or expose a reclamation decision. It must never be
used to decide reachability or trigger local-cycle collection. A complete
runtime handoff/root publication contract is required before any such use.

## Minimum follow-up test contract

- Real bytecode self-cycle and mutual-cycle reproduction through untyped
  / verifier-bypassing paths (the typed language rejects structural cycles).
- Uncounted `Move` register alias, closure environment, deep spill, and
  active interpreter/JIT-state pin tests.
- Suspended workflow, nested effect continuation, single-shot state and
  wake/resume tests.
- Exclusive mailbox snapshot across all queues, staged buffers and
  active candidates: preserve FIFO, current match, capacity, and rejection
  semantics; prove no concurrent sends or root handoff can race with snapshot.
- Separate concurrent SegQueue admission/root barrier and transfer races
  for any collection that does not run at a true exclusive safepoint.
- Individually owned cross-shard envelopes, retained fetch retries and full
  cross-shard channel handoff, including interrupted delivery.
- Actor state/event/query, exact live payload provenance, foreign-held/retired-
  owner, stale pointer reuse across a new allocation generation, pinned object
  and adversarial continuation-depth tests.
- Candidate work budget, collection pauses, RSS and allocation overhead on
  same-host A/B runs.

Do not silently treat a non-rooted foreign-edge graph as a heap reachability
proof, and do not use `foreign_count - outgoing_edge_count` alone as a
reclamation criterion.

## Focused validation

```sh
cargo fmt --all -- --check
cargo test --locked orca_execution_root_visitor_tests
cargo test --locked orca_actor_stored_root_visitor_tests
cargo test --locked actor_heap_provenance_rejects_foreign_interior_and_freed_pointers
cargo test --locked orca_mailbox_root_visitor_tests
cargo test --locked exclusive_root_snapshot
cargo test --locked exclusive_actor_value_snapshot
cargo test --locked orca_diagnostic_root_audit
cargo test --locked orca_pending_fetch_root_visitor
cargo test --locked orca_cross_shard_envelope_root_visitor
cargo test --locked --lib
cargo test --locked --features wasm-backend --lib
cargo clippy --locked --all-targets --all-features -- -D warnings
```

No tests have been executed locally by this draft's authoring environment.
