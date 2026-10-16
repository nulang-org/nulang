# Web reactive scopes (experimental)

This document describes the browser micro-runtime's opt-in local signal
identity contract. It is a **runtime foundation**, not a compiler feature,
a security boundary, or a production-stable component API.

## API

```javascript
const scope = window.nulang.mount(element, { localSignals: true });
const status = scope.signal("status");

status.get();
status.set("Ready");
scope.dispose();
```

- Existing `window.nulang.signal(name)` remains global.
- Existing `window.nulang.mount(root)` retains global signal bindings.
- `localSignals: true` creates an isolated identity for each signal name
  within this mount instance. Names may repeat across mounts without collision.
- A local scope initializes its signals from the first matching SSR text or
  safe attribute binding in its subtree. Multiple sinks for the same name
  share that identity within the scope.
- Signal changes use the existing synchronous index-based DOM updates and
  `window.nulang.batch(callback)`. The runtime creates no alternate scheduler.
- Disposing a scope removes its bindings and event listeners, cancels pending
  batched writes for its local identities, and invalidates its signal handles.
  Remounting creates fresh local identities.
- Repeating a mount in the same mode returns the same scope. Attempting to
  change its mode is an error.
- An opt-in local mount fails rather than silently adopting a DOM node owned
  by a different scope. This includes elements already bound during document
  hydration.

Identity is represented by a JavaScript `Symbol` scoped to the mount,
**not a string prefix**, avoiding collisions with global names or
`__proto__`-like keys.

## Limitations

Automatic document hydration continues to register the existing global
bindings. Component-local scopes currently work on explicitly mounted,
unowned subtrees; there is no compiler-generated component scope marker
or automatic nested-scope discovery yet. A component's local scope must
therefore be mounted before another scope claims its elements.

The compiler's existing graph still refers to source signal names.
This PR does **not** implement compiler-resolved per-component signal
IDs, local signal declarations in Nulang source, keyed reconciliation,
derived computations, dynamic dependency tracking, or automatic disposal
after DOM detachment.

A future compiler-owned binding IR should map resolved source declarations
and component instance boundaries onto these runtime identities. That work
must preserve the existing authoritative typed/effect-checked metadata and
avoid a second source parser or reactivity semantics.

The global signal surface is retained for backwards compatibility. Scoped
identity is a state isolation mechanism, not authorization or tenant
isolation. Event action placement remains governed by existing effect
checks, not by the scope-local signal option.

## Verification

```sh
node --test tests/web_reactivity_runtime.test.cjs
node --test tests/web_reactivity_browser.test.cjs
cargo fmt --all -- --check
cargo check --all-targets
cargo test web::reactivity::tests --lib
cargo test --lib
```

The compiled-SSR Chromium test additionally requires `NULANG_BIN`.
Both the parent PR stack and this exact PR head require executable CI
before promotion.
