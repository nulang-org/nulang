# Experimental Web reactive attribute bindings

Status: experimental compiler/runtime slice. Not part of Nulang's Stable or
Frozen language surface. Do not promote before the
[semantic stabilization gate](SEMANTIC_STABILIZATION_CONTRACT.md) passes.

## Intent

Nulang Web already emits server-rendered HTML and uses compiler-selected
`data-signal` nodes for directly interpolated signal text. This slice supports
direct signal references in two inert HTML attributes:

- `title={signalName}`
- `aria-label={signalName}`

The JSX attribute parser accepts hyphenated attribute names in the HTML
grammar only; ordinary identifiers are unchanged.

## Compiler and runtime contract

The compiler adds one marker alongside each supported HTML attribute:

| Source attribute | Generated marker |
| --- | --- |
| `title={label}` | `data-nulang-bind-title="label"` |
| `aria-label={label}` | `data-nulang-bind-aria-label="label"` |

`SignalGraph` records `AttributeRead` nodes with `signal`, `path`, and
`attribute`. The existing `nulang-ui/1` projection and signal-patch generation
reuse this compiler-owned metadata, with distinct patch target identities for
text and attribute bindings. No second source parser is introduced.

The browser micro-runtime discovers each marker once at hydration, captures
the initial server-rendered attribute as string state for attribute-only
signals, and updates only the registered `setAttribute` targets on subsequent
`window.nulang.signal(name).set(value)` calls. It also continues updating
direct text bindings. Pages without signal/action/attribute markers do not
receive the client script.

## Security and limits

This is a deliberately small allowlist. URL-bearing attributes (`href`,
`src`, etc.), event attributes, `style`, and raw HTML are **not** reactive
binding targets. Unknown or computed attribute expressions remain server
rendered; their signal references are not emitted as live DOM patch targets.
The compiler does not attempt to evaluate arbitrary Nulang expressions in
JavaScript.

The server must still escape HTML and attributes correctly. Client updates
use `setAttribute` on names selected by the compiler, never `innerHTML`.
No implicit browser authority is added to Nulang's effect system.

The current micro-runtime serializes signal values as strings for DOM text
and these attributes. Derived computations, structured values, keyed list
diffing, async resource loading, and general-purpose attribute policies
remain future work. This change does not alter Native UI or desktop lowering.

This slice must be validated against rendered HTML, emitted JavaScript,
compiler IR/patch semantics, accessibility behavior, and the project's
exact-head CI before it is considered production ready.


## Explicit synchronous update batches

The experimental browser micro-runtime supports `window.nulang.batch(callback)`
to coalesce synchronous signal writes without introducing scheduled rendering or
changing the behavior of ordinary calls to `signal(name).set(value)`.

```javascript
const status = window.nulang.signal('status');
window.nulang.batch(() => {
  status.set('saving');
  status.set('saved');
  // status.get() === 'saved', while the DOM still has its prior value.
});
// Bound DOM text/attributes now show 'saved', after one refresh for 'status'.
```

- Signal reads observe the latest value immediately.
- Outside a batch, writes still update bound DOM nodes synchronously.
- Within a batch, only the final value of each changed signal is rendered, once
  at the outermost batch boundary; nested batches do not flush early.
- Assigning the same value by `Object.is` does not trigger a DOM write.
- The callback's result is returned. A thrown callback error is propagated
  **after** the queued DOM updates are flushed.
- The callback must be a **synchronous function**. An asynchronous callback
  cannot extend the batch across `await`; subsequent writes after suspension
  execute outside the batch.
- Batching does not add reactive expression evaluation, permission changes,
  or new DOM sinks. Attribute updates remain restricted to the explicit safe
  allowlist described above.

Run the dependency-free behavioral test with:

```sh
node --test tests/web_reactivity_runtime.test.cjs
```

This VM-based test does not replace actual browser, Rust compiler, SSR, or
end-to-end conformance verification.
