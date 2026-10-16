# Experimental Nulang Web conditional-region contract

Status: **experimental, opt-in browser coordination**. This is a compiler/SSR
identity contract and a lifecycle-safe DOM replacement API, **not** automatic
client-side evaluation of Nulang `if` expressions.

## Emission

When an `if` guard is a directly referenced, module-declared `signal` and
**both** branches are direct `el(tag, attrs, children)` expressions, the Web
rewrite attaches these reserved attributes to each alternative's root:

| Attribute | Meaning |
| --- | --- |
| `data-nulang-if="visible"` | Identifies the Boolean signal controlling the branch |
| `data-nulang-branch="then"` or `"else"` | Identifies this particular alternative |

For example:

```nulang
signal visible: Bool = true

fn card() -> Html {
    if visible then <section>Shown</section> else <aside>Hidden</aside>
}
```

Only the evaluated SSR branch appears in the HTML. The compiler **does not**
render the inactive branch, move hidden/server effects to the browser, or
inject a wrapper that could break table/select/other HTML contexts. An
unrecognized branch root, missing alternative, non-signal guard, or conflicting
user-supplied reserved attribute leaves the condition unchanged.

The static HTML injector loads `/app.client.js` for documents with this
conditional metadata, even when they have no ordinary text bindings.

## Browser coordination

The client micro-runtime hydrates the initial Boolean value from the active
SSR branch. `window.nulang.signal("visible").get()` returns `true` for
`then` and `false` for `else`, unless the signal was already established.

`window.nulang.replaceConditional(currentRoot, nextRoot)` requires the caller
to provide a **detached DOM element**, already built or supplied by a trusted
renderer. Both roots must identify the same conditional signal and opposite
branches. It delegates DOM mutation and lifecycle disposal/hydration to
`replaceRegion`. After a successful swap, it publishes the new Boolean
signal value and refreshes bound text/attributes. Precondition failures leave
the current DOM untouched; failed replacement attempts use the region
replacement rollback path and retain the previous signal state.

This method does **not** synthesize or deserialize HTML; it cannot reconstruct
Nulang source, execute server effect handlers, or choose a branch automatically
when a signal is changed. Consumers must explicitly request an alternate
branch and own the lifetime of any detached DOM they supply.

### Deliberate exclusions

- No automatic subscription from `signal.set` to conditional rerendering.
- No serialization or pre-execution of the inactive server branch.
- No arbitrary expression guards, match expressions, or nested block roots.
- No keyed list reconciliation, transitions, or DOM diffing.
- No authorization decisions based on client-side `data-nulang-*` values.
- No untrusted HTML string insertion.

The markers describe **render identity**, not an authority or security
boundary. Server-side authorization still governs all protected content and
effects.

## Validation gates

- Focused Rust rewrite tests: only safe direct-root signal conditions gain
  markers; non-signal and unsupported branches are unchanged.
- `test_cmd_build_web_signal_hydration`: compiled SSR must emit only the
  active branch and its identity.
- `node scripts/tests/web_conditional_regions.mjs`: initial Boolean
  hydration, explicit alternate branch activation, identity mismatch
  rejection, event disposal, and failed replacement preservation.
- Full exact-head Cargo CI and compiled-page browser tests remain required
  before enabling automatic compiler-driven conditional rerendering.

Dependency sequence: reactive attributes (#1849), scoped lifecycle (#1856),
DOM replacement (#1859), this conditional-region contract. Integrate the
independent form submit fix (#1853) before promotion.
