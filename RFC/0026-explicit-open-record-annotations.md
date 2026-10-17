# RFC 0026: Explicit Open Record Type Annotations

- **Status:** Proposed
- **Tier:** Experimental (candidate for Stable)
- **Date:** 2026-10-12

## Problem

Nulang infers open record rows from field access, but adding explicit record
field annotations currently closes the record and rejects extra fields.
This penalizes typed public APIs and makes structurally typed SDKs awkward.

## Proposal

Allow an optional trailing `..` row marker in an inline record type:

```nulang
fn full_name(p: { first: String, last: String, .. }) -> String {
    p.first + " " + p.last
}
```

- Named required fields retain their normal types and checking.
- Extra caller fields are permitted.
- The row tail must be the last member and is inferred, not named.
- Records without a row marker remain closed with identical semantics.
- Initial implementation supports inline type annotations; named
  record declarations remain closed.

## Implementation

The parser consumes the existing `DotDot` token in the record type branch,
and returns `Type::record_open(fields, TypeVar::fresh())`. Type inference
uses the existing open-row unifier. Type display renders the synthetic
row-tail pseudo-field as `..` rather than showing a compiler-internal name.

## Experimental grammar

```ebnf
open_record_type = "{" [ field_type { "," field_type } "," ] ".." "}" ;
field_type = IDENT ":" type ;
```

## Compatibility

No Frozen Core production is changed or removed. The extension is additive
and remains Experimental until an RFC stabilizes it, following RFC 0002.
No runtime record layout change is needed.

## Risks and validation

Verify multi-call inference with different extra field sets, rejection of
missing or wrong-typed required fields, parser negative cases, formatter
idempotence, and compiler conformance at the exact branch head.

## Non-goals

Named row variables (`..R`), nominal subtyping, class syntax, or changing
actor-reference protocols are intentionally excluded.

## Promotion

Remain Experimental until exact-head CI, conformance, editor formatting
and RFC approval demonstrate compatibility with Frozen Core.
