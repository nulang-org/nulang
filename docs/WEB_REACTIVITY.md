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
