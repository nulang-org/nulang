# RFC 0017: Time-Travel Debugging & Deterministic Replayability

- **Status:** Draft
- **Tier:** Experimental
- **Author:** AI Assistant
- **Created:** 2026-08-12
- **Resolved:** (pending)
- **Language-version at effect:** N/A (Runtime/Tooling change)
- **Supersedes:** none
- **Superseded by:** none

## Summary

Introduce runtime primitives for Event Log Sourcing of actor mailboxes, enabling deterministic replayability and local time-travel debugging. By persisting incoming actor messages and non-deterministic seeds to an append-only log, developers can locally reconstruct distributed race conditions and step backward/forward through an actor's execution history.

## Motivation

Debugging distributed systems and actor-based concurrency is notoriously difficult due to non-determinism (message arrival order, network latency, distributed race conditions). While Nulang provides strict actor isolation, debugging edge cases currently relies on tracing and traditional print statements.

Because Nulang combines lightweight actors with reduction-bounded scheduling and pure message-passing (where payloads are `Arc<Vec<Value>>` and strings are interned), we have a distinct advantage: an actor's execution is entirely deterministic based on its initial state and the exact sequence of messages it processes. By recording this sequence, we can recreate the exact execution path locally, allowing a developer to step through state transitions backward and forward.

## Design

### 1. Message Event Logging (Event Log Sourcing)

We introduce a configurable replay log to the actor runtime. When an actor is spawned with replay enabled (`ActorOptions::enable_replay`), the runtime hooks into the message consumption pipeline.

- **Hook Point:** Inside `src/runtime/scheduler.rs` during `step_actor`, immediately after `mailbox.pop()` or `mailbox.receive_match()`.
- **Data Logged:** The `Message` struct (`behavior_id`, `payload`, `sender`, `priority`, `trace_id`) is serialized alongside a monotonic logical sequence number and the current PRNG seed (if applicable).
- **Serialization:** Because `Value` payloads might contain local heap pointers that become invalid across reboots, messages destined for the replay log are serialized using the existing portable format in `src/runtime/heap_serialize.rs`.
- **Storage:** The serialized messages are appended to the existing `PersistenceStore` (e.g., SQLite/libsql) in a new table `actor_replay_log`.

### 2. Periodic Checkpointing

Replaying an actor from its genesis for long-lived actors is computationally expensive. We will leverage Nulang's existing durable continuation mechanism.
- The `OrcaGc` / `ActorHeap` will be snapshotted every $N$ reductions (e.g., $N=1000$) using `src/runtime/heap_serialize.rs`.
- These checkpoints act as keyframes.

### 3. Time-Travel Execution Engine

To debug an actor, the developer launches the Nulang CLI in replay mode:
`nulang run --debug-replay <actor_id> --at-sequence <sequence_num>`

The runtime will:
1. **Restore:** Locate the nearest checkpoint $\le$ `sequence_num` and deserialize the `ActorHeap` and VM frames.
2. **Replay:** Fetch the ordered log of messages from the checkpoint's sequence up to `sequence_num`.
3. **Execute:** Feed these messages sequentially into `step_actor`, running the VM in a **sandboxed mode** where external side-effects (network I/O, outgoing messages to other actors) are dropped or mocked.

### 4. Debugger Integration

The DAP server (`src/dap/mod.rs` or `src/lsp/mod.rs`) will be extended with standard reverse-debugging commands:
- `Reverse Continue`: Load the nearest prior checkpoint and replay forward to the previous breakpoint.
- `Step Back`: Replay from a checkpoint up to `current_sequence - 1`.

## Tier Classification

This is an **Experimental** runtime and tooling feature. It does not affect the language grammar or stable APIs. It introduces tooling enhancements for developers.

## Backwards Compatibility

This RFC does not break existing programs. Logging is strictly opt-in via a new `ActorOptions` flag or debugging CLI flag to avoid performance overhead in production environments unless explicitly requested.

## Alternatives Considered

- **Language-Level Event Sourcing (RFC 0007):** Using `emit` and `events` blocks. *Rejected for this use-case* because RFC 0007 focuses on domain logic and state field mutations. Time-travel debugging requires recording *all* inputs (messages) transparently at the VM level without developer boilerplate.
- **Full VM State Trace (rr-style):** Recording every instruction and memory write. *Rejected* because Nulang's actor isolation means we only need to record the inputs (messages) to achieve determinism, which is vastly cheaper in storage and overhead.

## Open Questions

- How do we handle time-based effects (e.g., `Timer.sleep` or timeouts) during replay? We will likely need to record the resolved timestamp of the timer in the event log so the replayed actor observes the exact same time.
- Should the replay log be stored in the same `PersistenceStore` database as durable state, or a separate debug-specific local store?

## Resolution

(pending)
