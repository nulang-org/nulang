# Keyed UI patches and input synchronization (experimental)

This document describes the incremental LiveView-inspired protocol slice in
`nulang-ui-protocol`. It does **not** turn Nulang into a WebSocket-only framework
and does not add a second HTML renderer.

## Identity-preserving document diffs

`UiDocument::diff(&next)` validates both documents and produces a deterministic
`UiPatch` with an increasing revision and the same document ID.

- `NodeId` is the stable key, **not** a list position. Reordering existing
  children emits `ReplaceChildren` without recreating each row.
- Properties emit `SetProperty` / `RemoveProperty`; actions emit
  `ReplaceActions`; new or retyped nodes emit `UpsertNode`.
- Disconnected nodes emit `RemoveNode` after parent references have changed.
- `UiDocument::apply_patch` atomically validates the final tree and leaves the
  old document unchanged when a patch violates a tree/revision invariant.
- Host DOM adapters must perform keyed moves by `NodeId` and preserve
  focused node identity, selection, composition and scroll state. The Rust
  protocol alone cannot preserve browser DOM state.

## Input revision protocol

`UiHostState` maintains a host-side document plus outstanding local input edits.

1. A host locally increments a monotonically increasing sequence for each
   `(NodeId, property)` pair. The only locally mutable properties are
   `value` and `checked`.
2. It calls `record_input(node_id, property, sequence, value)` before sending
   the event. Edits become immediately visible in the host document.
3. The event carries the same input identity and sequence to the server, along
   with the existing action correlation/idempotency metadata. An application
   must bind and authenticate this metadata to the authorized UI session.
4. **Only after the server has incorporated an event** may a later server
   `UiPatch` include an `acknowledged_inputs` record with that
   `node_id`, `property`, and `sequence`.
5. `apply_server_patch` skips duplicate or stale revisions. It rejects gaps
   as `RevisionMismatch` (request a fresh snapshot), and reapplies any local
   edits newer than their explicit acknowledgement. Other server properties
   are not blocked.
6. Acknowledgements are cumulative high-water marks per input. An old ack
   cannot unlock a newer local edit. If a node disappears or changes kind,
   pending edits for that identity are discarded. Reload/reconnect snapshot
   reconciliation needs a separate browser integration and should not silently
   replay mutations.

`acknowledged_inputs` is an optional addition to `nulang-ui/1`:
existing snapshots, patches, and action requests remain valid, with no
implicit acknowledgement when the field is absent. Sequences use the existing
decimal-string `Revision` representation to retain full `u64` precision in
JavaScript and JSON.

The existing `ActionRequest.payload` is a `WireValue` and can carry input
identity/sequence as application data until there is a compiler-owned,
verified input event binding. **Do not** treat client-submitted sequence,
revision, or acknowledgement values as authorization to run an action.
Server handlers must validate the session, action, input membership and
idempotency constraints before incorporating or acknowledging an event.

## Tests

`cargo test -p nulang-ui-protocol`

Regression tests cover keyed reorders, granular property changes, node
insertion/removal, action changes, retyping, revision/document mismatches,
input event interleavings, stale duplicate patches, gap rejection,
acknowledgement precision/compatibility, and atomic validation failures.

## Remaining work before production browser integration

- Compiler emission of stable NodeIds for keyed JSX/comprehension blocks.
- Browser DOM adapter that executes keyed moves and preserves selection,
  focus, IME composition, scrolling, and accessibility state.
- An authenticated server event binder which echoes validated input
  high-water marks in generated patches; add host/server end-to-end tests.
- Snapshot resynchronization, reconnect semantics, bounded memory, and
  payload/CPU benchmarks under reordered and dropped network responses.
