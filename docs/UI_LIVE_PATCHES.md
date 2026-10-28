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

## Opt-in browser DOM host and explicit JSX keys (follow-up)

`crates/nulang-ui-protocol/browser/nulang-ui-dom.js` provides a small,
dependency-free browser host for **semantic** UI documents. The web build
includes it in the generated `app.client.js` alongside the existing
signal/action micro-runtime, exposing `window.nulangUiDom.createHost(root,
snapshot)`.

```js
const host = window.nulangUiDom.createHost(
  document.getElementById("nulang-root"),
  initialUiDocument
);
host.onInput = ({node_id, property, sequence, value}) => {
  // Send through an authenticated action transport. The sequence alone is
  // never a credential and the transport must bind it to the live session.
};
host.applyMessage({
  type: "patch",
  protocol: "nulang-ui-msg/1",
  patch: serverPatch
});
```

The DOM host validates a restricted set of semantic tags and properties,
uses `createElement` and `textContent` (never `innerHTML`), checks URL
schemes and document/revision invariants, reconciles keyed nodes by
`NodeId` without recreating moved nodes, and preserves focus and selection
when safe. It records local `value` / `checked` edits and honors cumulative
server acknowledgements. It queues up to 32 patches while an IME composition
is active, applying them in order when composition ends. A queue overflow
or gap requires an explicit snapshot resynchronization; the host does not
silently overwrite edits or fabricate an acknowledgement.

The compiler's experimental web rewrite changes explicit JSX
`key={expression}` to `data-nulang-key={expression}` so SSR markup retains
the stable application-provided key. Keys are scoped to the parent list.
The host's `NodeId` is the canonical identity for protocol patches. **The
compiler does not yet materialize a full UI tree from JSX or derive a canonical
NodeId from every keyed comprehension**; the attribute is a marker for the
future compiler/SSR bridge, not a substitute for a unique protocol identity.

**Browser verification** (real headless Chromium, no JS package dependencies):

```sh
python3 crates/nulang-ui-protocol/browser/test_dom_host.py
```

The runner starts an ephemeral localhost server and a headless Chromium
session, asserts `data-test-status="passed"` and `ALL PASS`, and cleans up
both resources. Set `NULANG_BROWSER_TEST_TMP` to a short writable directory
if temporary-file quotas or long Chromium socket paths are an issue.
A real browser is required because Node alone lacks a DOM implementation.

**Remaining work:** compiler/server generation of matching NodeIds from
keyed loops; authenticated patch delivery and acknowledgements; snapshot
recovery/reconnect; full SSR-to-DOM hydration; property/ARIA semantics for
additional native controls; browser E2E tests with a real Nulang application.
No transport or server-action authorization is added by this opt-in host.
