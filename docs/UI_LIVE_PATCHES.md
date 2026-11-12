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

## Parent-scoped key/SSR bridge (experimental follow-up)

`document_from_keyed_tree` provides a server-side construction path for a
UI protocol document whose `NodeId` values match those calculated during
browser SSR adoption:

- Root is always `root` and cannot have a key.
- For keyed siblings, append `/k` plus lowercase hex of the raw UTF-8 bytes
  of the key: `"é"` becomes `root/kc3a9`.
- For unkeyed siblings, append `/p` and the zero-based position among **all**
  element children (including keyed ones).
- The same algorithm applies recursively under each parent's full ID.
  Keyed moves within a parent preserve identity; moving a keyed row between
  different parents does not.
- Duplicate sibling keys, empty/control-character keys, and oversized trees
  are rejected. Maximum key size: 256 UTF-8 bytes; maximum generated ID:
  1024 bytes; maximum depth: 64; maximum nodes: 10,000.

Server code can use the new `KeyedNode` builder:

```rust
use nulang_ui_protocol::{
    document_from_keyed_tree, KeyedNode, Revision, WireValue
};

let tree = KeyedNode::new("ul").with_children(vec![
    KeyedNode::new("li").with_key("task-42")
        .with_property("text", WireValue::from("Review proposal")),
]);
let document = document_from_keyed_tree("tasks", Revision(1), &tree)?;
```

The browser can adopt a matching **already-rendered** DOM tree, preserving
the original element objects:

```html
<div id="mount"><ul><li data-nulang-key="task-42">Review proposal</li></ul></div>
```

```js
const host = window.nulangUiDom.createHostFromDom(
  document.getElementById("mount"),
  { documentId: "tasks", revision: "1" }
);
// host.element("root/k7461736b2d3432") is the original <li>.
```

This adapter currently supports a strict allowlist of native DOM elements and
attributes, text-only leaf content, and element-only container children.
Mixed text with nested elements requires compiler-owned text ranges and is
rejected rather than silently producing incompatible identities.

The browser also supports explicit same-document snapshot recovery:

```js
host.resync(trustedSnapshot);                       // allowed if no drafts
host.resync(trustedSnapshot, {discardPending:true}); // explicit draft loss
```

`UiHostState::resync(snapshot, discard_pending)` provides matching
renderer-neutral Rust behavior. Both implementations reject older revisions,
cross-document snapshots, and implicit loss of unacknowledged local edits.
Input sequence high-water marks survive same-document snapshots.

These IDs are deterministic—not access-control tokens. The server must still
verify session/tenant authority, CSRF/Origin policy, action binding, and input
high-water marks before delivering a patch or an acknowledgement. **No
authenticated SSE/WebSocket/HTTP patch transport is introduced by this change.**
The compiler's `key={...}` marker still needs a full JSX-to-`KeyedNode`
lowering path before this works automatically for arbitrary Nulang views.

Focused regressions (the Chromium test is also run in the existing
CI **Docs site & browser playground** job):

```sh
cargo test -p nulang-ui-protocol
python3 crates/nulang-ui-protocol/browser/test_dom_host.py
```

## Compiler-owned UI template plan (experimental)

The Nulang web frontend now provides
`nulang::web::ui_template::UiTemplate::compile(&ast, "view")`.
This lowers a **restricted, effect-free subset** of the existing parsed JSX
`el(tag, attrs, children)` tree into a serializable `UiTemplate` plan.
No second JSX parser or virtual DOM was introduced.

The supported subset includes literal string/boolean/integer values, variable
and record-field paths, ordinary nested elements, text-only leaves, and keyed
`for item in rows { <li key={item.id}>{item.label}</li> }` comprehensions.
All loop-produced elements must have an explicit key. Browser-unsafe tags
or attributes, scriptable URL schemes, effectful expressions, arbitrary
function calls, and mixed text/element children are unsupported and fail
closed when explicitly compiled.

`UiTemplate::render(document_id, revision, &bindings)` binds server-supplied
`WireValue` objects and arrays into a `KeyedNode` tree, which passes through
the same `document_from_keyed_tree` builder as SSR. This produces the stable
parent-scoped IDs used by the browser host, allowing
`UiDocument::diff` to emit a `ReplaceChildren` instead of recreating rows.

When compiling a web source with `--rewrite-signals` (including `nula
build --web`), the CLI writes `.nula/app.ui-templates.json` beside
the compiled `.nula/build.nbc` artifact, **not** alongside public
`app.client.js`. The versioned manifest contains only functions whose JSX
root can be conservatively lowered; unsupported functions remain on the
existing HTML renderer. It contains **no bound request/user data, authorization
tokens, or executable handlers**. The server decides whether to use a plan,
binds validated values, and authenticates any resulting action independently.

The generated template manifest is an experimental sidecar, not yet an
automatic runtime route. It does not authenticate sessions, emit live patches,
or stream updates. A following integration should consume the manifest in a
session-authorized server renderer and pair it with the existing action
contract and verified SSE/HTTP transport. Do not allow a client to select
arbitrary templates or bind privileged values without server-side policy.

## Scoped server sessions and authoritative input acknowledgement (follow-up)

`nulang::web::ui_live_session::UiLiveSession` binds a compiler-owned template
to a **single, server-owned** tenant, subject, session, and document. Unlike
a general public page renderer, its `snapshot` and `commit` methods require
the host application's trusted authorization callback **on each invocation**.

```rust
use nulang::web::ui_live_session::{UiLiveSession, UiSessionScope};
use std::path::Path;

let scope = UiSessionScope::new(
    trusted_tenant, trusted_subject, trusted_session, "task-list"
);
let session = UiLiveSession::open_compiled_manifest(
    Path::new(".nula/app.ui-templates.json"),
    "tasks",
    scope,
    &trusted_bindings,
    &|expected| verify_current_request_and_view_permission(expected),
)?;
let snapshot = session.snapshot(&|expected| verify_current_request_and_view_permission(expected))?;
```

`verify_current_request_and_view_permission` represents an application
function, **not a built-in authentication mechanism**. It must validate the
current request's actual signed credential (or another trusted, non-forgeable
identity), session and revocation status, tenant, subject, specific document
and compiled view name. Never authorize based only on client-supplied
`document_id`, session IDs, headers, input sequences, or path strings.

`UiLiveSession::commit(&bindings, &inputs, authorize, server_accepted)`:

- Rejects revoked/unauthorized sessions before rendering or publishing.
- Renders the next authoritative document with a checked revision increment.
- Diffs against the last document to emit a `RuntimeToHostMessage::Patch`.
- Checks every proposed input acknowledgement against both the previous and
  new DOM node, control type, property/value and per-input sequence history.
- Calls `server_accepted(&inputs)` only after these checks. An acknowledgement
  is emitted **only when the trusted application handler confirms incorporation
  of the input into authoritative state**. The application's database write
  and acceptance check must themselves be transactional and idempotent.
- Commits the new document, acknowledgement high-water marks and revision
  together only after successful validation and acceptance. Failed commits do
  not mutate the UI session.
- Has a bounded number of tracked input identities and an explicit
  `revoke()` operation.

The server loader reads at most 1 MiB from the private compiled manifest.
Authorization happens **before** reading the file. Runtime rendering
revalidates the tag and attribute allowlist even for deserialized plans.

**Security/deployment boundary:** this coordinator does not start an
HTTP endpoint, allocate an SSE room, automatically connect a browser,
create an identity token, or attest server transaction success. Existing
draft PRs #2367 (signed tenant/SSE authorization) and #2364 (targeted-action
response concurrency) overlap in the HTTP server. Review their exact-head
CI and consolidate them before wiring this coordinator to a transport.
In particular, the currently unprotected legacy `/__nulang/sse/` route
must not be used for private UI patches. EventSource reconnect must
reauthorize every subscription, not replay cross-session history.

Focused integration tests in `tests/web_ui_live_session.rs` cover the
full compiler template → server session snapshot → ordered patch → Rust
host reducer path, denied/revoked access, failed renders, input acceptance
and replay rejection, private manifest loading, malformed/oversized
artifacts and corrupted template content. Browser reconciliation is tested
separately by the Chromium suite.
