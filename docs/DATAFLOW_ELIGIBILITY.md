# Experimental dataflow pure-expression screening

This is a compiler-side **candidate screen** for future optimization research,
not an authorization to execute, fuse, memoize, reorder, or parallelize code.

`src/dataflow_eligibility.rs` deliberately accepts a tiny AST subset:
literals, explicitly asserted immutable inputs, pure-looking arithmetic,
unary expressions, tuples, blocks, immutable `let` bindings and conditionals.
Every other construct (calls, lambdas, assignments, loops, captures not listed
as inputs, actor operations, effects, `consume`, `defer`, handlers, etc.) is
rejected. The canonical `EffectChecker` additionally must infer a **closed
empty** effect row. An empty row alone is never proof of purity because unresolved
and indirect calls may not be represented in that row.

## Canonically checked function entrypoint

`classify_checked_function_candidate(module, name)` finds exactly one
module-level function declaration, derives input names from its parameters,
and runs the existing Nulang type, effect, and capability analyzers before
screening the expression body. It requires each parameter to have an explicit
type and `val` reference capability; missing annotations are **not**
implicitly trusted. It also rejects default arguments, contracts, generic
parameters, implicit `using` parameters, and declared error types.

This improves over the low-level `classify_pure_candidate` API, whose
immutable input-name list is supplied by its caller. **Neither API uses
resolved symbol IDs or authorizes a rewrite.** The new entrypoint does
not modify the compiler pipeline or become part of an executable backend.
It screens only top-level function declarations and is intentionally
incomplete for other source forms.

## Critical limitations

- The input-name list is supplied by the caller. **This pass does not validate
  whether those bindings are immutable, owned, reference-capability safe, or
  stable at the call site.** A positive result is `Candidate`, not `Eligible`.
- A candidate is not a proof of deterministic results: traps, resource budgets,
  allocation failures and evaluation order can be observable.
- The screen is **not wired into** source compilation, MIR passes, runtime
  execution, or dataflow callback admission. Existing semantics stay unchanged.
- Closed-empty effect rows, immutable AST syntax and prevalidated references
  are all necessary but not collectively sufficient for general transformations.
- No Nulang source syntax, wire/bytecode format, or stable API is introduced.

## Validation

Focused tests:

```sh
cargo test --locked --lib dataflow_eligibility::tests
cargo test --locked --lib dataflow::tests
cargo fmt --all --check
```

Repository-required lint and cross-backend workflows must pass before merge.

## Follow-up proof obligations

1. Obtain input identities/types/capabilities from canonical resolver and type
   checker, not a caller-supplied string allowlist.
2. Distinguish evaluated expressions from closures, type-level assumptions,
   actor-local state and delayed execution.
3. Prove left-to-right error/trap behavior, short-circuit semantics and any
   allocation-observable behavior for each contemplated rewrite.
4. Execute a differential conformance corpus through the reference bytecode
   path and each supported backend before any optimization consumes a positive
   candidate.
5. Only then introduce a representation-independent graph IR and benchmark
   whether a new optimization provides benefit over existing MIR/JIT passes.

See [dataflow prototype](DATAFLOW_PROTOTYPE.md) and [RFC 0024](../RFC/0024-orthogonal-execution-model.md).
