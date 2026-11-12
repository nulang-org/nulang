# Browser-free UI protocol testing (experimental)

Nulang's `nulang-ui/1` protocol now has a small, deterministic test harness
in `nulang_ui_protocol::testing::UiTestHarness`. It is inspired by Phoenix
LiveViewTest's fast interaction assertions, but **does not implement LiveView**.

It operates on the existing renderer-neutral `UiDocument`, `UiPatch`,
`RuntimeToHostMessage`, and `HostToRuntimeMessage` types. No browser,
WebSocket, network listener, runtime actor, or new UI protocol is required.

## Example

```rust
use nulang_ui_protocol::{
    testing::UiTestHarness, NodeId, Revision, RuntimeToHostMessage, UiDocument, UiNode,
};

let root = UiNode::new("root", "column");
let document = UiDocument::new(
    "example", Revision(1), NodeId::from("root"), vec![root],
);
let mut host = UiTestHarness::mount(RuntimeToHostMessage::snapshot(document)).unwrap();
assert_eq!(host.node("root").unwrap().kind, "column");
// Pass RuntimeToHostMessage::patch(...) to host.receive(...) to test updates.
```

The harness:
- Validates the initial snapshot and every incoming message.
- Applies patches using the protocol's atomic document transition.
- Accepts identical reconnect snapshots; rejects stale or conflicting snapshots
  and snapshots from a different document.
- Looks up semantic nodes by stable NodeId without DOM selectors.
- Builds an action envelope only for an unambiguous, declared **server** event,
  using the current document ID and revision and caller-supplied correlation and
  idempotency keys.
- Returns typed errors instead of mutating state when an operation fails.

## Run the focused tests

```sh
cargo test --locked -p nulang-ui-protocol --test live_view_harness
cargo test --locked -p nulang-ui-protocol
```

## What it does not prove

This harness does **not** run compiled Nulang handlers, perform authorization,
submit HTTP forms, emulate focus/IME state, reconcile the browser DOM, or prove
security of the experimental development server. It does not replace browser
tests or exact-head integration/security qualification.

The next stage should connect compiler-backed views and authenticated UI
session coordinators to this test API *after their existing draft integration
stack has passed its own exact-head tests*. Avoid duplicating the draft
`UiHostState` reducer or creating a second patch protocol.

## Compiler-to-browser wire contract (stacked integration only)

The session integration stack includes a checked-in browser fixture at
`crates/nulang-ui-protocol/browser/live-session-fixture.json`. It is generated
from real parsed Nulang JSX views and `UiLiveSession` output, not handwritten
protocol samples. A Rust regression verifies the fixture against the current
compiler-produced snapshot, keyed list patch and input-acknowledged form patch.
Chromium then replays those same messages using the dependency-free DOM host.

```sh
cargo test --locked --no-default-features -p nulang \
  --test web_ui_harness_integration
python3 crates/nulang-ui-protocol/browser/test_dom_host.py
```

These tests guard the Rust/JavaScript serialization boundary: `UiNode` omits
empty `properties` and `children` collections on the wire; the browser
normalizes only absent collections to empty values, while rejecting explicit
`null`, arrays where objects are expected, and other malformed fields.

The existing `Docs site & browser playground` CI job already invokes the
Chromium browser runner. **It does not validate authenticated HTTP/SSE
subscriptions**; the host verifier and transport security are separate gates.
