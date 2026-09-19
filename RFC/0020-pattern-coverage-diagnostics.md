# RFC 0020: Pattern Coverage Diagnostics

- **Status:** Draft
- **Tier:** Experimental
- **Author:** Nulang Core Team
- **Created:** 2026-09-16
- **Resolved:** (pending)
- **Language-version at effect:** n/a (diagnostic-only in 1.x)
- **Supersedes:** none
- **Superseded by:** none

## Summary

Add conservative compile-time diagnostics for provably non-exhaustive and
redundant `match` arms without changing Nulang 1.x program validity or runtime
semantics. Finite declared variants are analysed statically and produce warning
`W0201` for missing constructor coverage and `W0202` for redundant arms.
Unknown or infinite domains continue to rely on the existing runtime
non-exhaustive-match fallback. `--deny-warnings` provides opt-in strictness.

## Motivation

Nulang currently has three conflicting signals around pattern matching:

1. RFC 0002 freezes `match` syntax, typing rules, and evaluation semantics and
   guarantees that every Core program valid at the freeze remains valid.
2. `ARCHITECTURE.md` and the behavioral conformance suite accurately document
   current behavior: a non-exhaustive match compiles and can fail at runtime.
3. The language safety documentation claims missing variant arms are compile-
   time errors, which is stronger than the frozen implementation contract.

The compiler should detect obvious coverage defects, but changing a formerly
valid Core program into an unconditional type error would violate RFC 0002.
Warnings close most of the safety gap while preserving compatibility.

The initial implementation lives in `src/pattern_coverage.rs`. It is designed
as a conservative proof engine: when it cannot prove a property, it emits no
coverage conclusion and leaves runtime behavior unchanged.

## Design

### 1. Coverage report

`src/pattern_coverage.rs` exposes:

```rust
pub struct CoverageReport {
    pub missing: Vec<String>,
    pub redundant_arms: Vec<usize>,
}

pub fn analyze_variant_match(
    scrutinee_ty: &Type,
    arms: &[(Pattern, bool)],
) -> Option<CoverageReport>;
```

The boolean records whether the arm is guarded. `None` means the analysis does
not claim the domain is statically finite/decidable.

### 2. Initial proof domain

The first implementation analyses only closed `Type::Variant` scrutinees
(after peeling reference wrappers).

An unguarded constructor arm covers a constructor when:

- the declared constructor is nullary and the pattern is nullary; or
- the declared constructor has a payload and the payload pattern is an
  unconditional wildcard/variable (possibly through aliases).

An unguarded top-level wildcard/variable is a catch-all. Guarded arms never
close a coverage hole because the guard may evaluate to false.

Tuple and record payload patterns deliberately do not prove full constructor
coverage in the initial implementation. The current pattern binder is
permissive about structured shapes; accepting those as proofs before exact
shape validation would permit false exhaustiveness conclusions.

### 3. Warnings

`warnings_for_variant_match` converts a report into `NuWarning` values:

- `W0201`: `non-exhaustive match: missing <witnesses>`
- `W0202`: `redundant match arm(s): <indexes> cannot be reached`

Witnesses use source-like constructor forms such as `Blue` and `Some(_)`.
Arm indexes in diagnostics are one-based.

Both warnings use the match expression's span initially. A later AST change
may attach spans directly to patterns/arms for more precise `W0202` locations.

### 4. Frontend integration

The typechecker remains authoritative for the scrutinee type. After ordinary
match arm typechecking succeeds, the frontend may run finite-variant coverage
analysis and append `W0201/W0202` to its warning stream.

Default compilation still succeeds. Existing MIR lowering retains its runtime
non-exhaustive fallback.

When `--deny-warnings` is supplied, the existing warning policy may escalate
these diagnostics just like deprecation warnings. This is opt-in strictness,
not a change to the default Frozen-Core language.

LSP diagnostics should surface the same warning codes. JSON diagnostic output
should preserve the same stable codes once frontend plumbing is complete.

### 5. Future complete pattern matrix

The coverage engine should evolve toward a Maranget-style usefulness/pattern-
matrix algorithm supporting nested variants, tuples, records, literals and
witness generation. Each extension must remain conservative: unsupported
shape means "unknown", never "exhaustive".

The runtime fallback remains required in Nulang 1.x even when the static pass
can prove many cases.

### 6. Hard errors

Making a provably non-exhaustive Core `match` an unconditional compile-time
error changes which frozen programs are valid. That change is out of scope for
this RFC and must not ship in Nulang 1.x.

A future Nulang 2.x RFC may make `W0201` deny-by-default or turn it into a type
error as part of a major-version Core revision. Such an RFC must define the
migration rule and update the Frozen Core contract explicitly.

## Tier Classification

Experimental diagnostic tooling on top of Frozen syntax/typing/evaluation.
The analysis does not alter the Frozen language contract in default mode.
Warning codes become stable once shipped because tooling may depend on them.

## Backwards Compatibility

Default behavior is backwards compatible:

- programs that compile today continue to compile;
- programs that fail only when an uncovered match value is executed retain
  that runtime behavior;
- bytecode/runtime fallback behavior is unchanged;
- no syntax or serialized format changes.

`--deny-warnings` is explicitly strict/opt-in and may reject a source file that
would otherwise compile, as it already does for warning categories.

## Alternatives Considered

### Make non-exhaustive finite variants a type error immediately

Rejected for Nulang 1.x. RFC 0002 freezes Core `match` validity and semantics,
and the current architecture/conformance contract permits runtime failure.
This would make previously valid Core source invalid without a major version.

### Do nothing and retain runtime-only failures

Rejected. Closed variants provide enough information for useful compile-time
feedback, and missing constructors are a common correctness defect.

### Remove the runtime fallback after static analysis exists

Rejected. The initial checker is deliberately incomplete, and removing the
fallback would weaken safety for unsupported/infinite-domain patterns. It also
changes frozen evaluation behavior.

### Treat guarded wildcard arms as exhaustive

Rejected. A guard can evaluate to false; counting it as exhaustive is
unsound.

## Open Questions

- Should `W0202` remain enabled by default when intentionally ordered guarded
  patterns become more expressive?
- Should the next AST revision attach a span to every `Pattern` or every match
  arm so redundancy warnings point at the exact arm instead of the match?
- Which Nulang 2.x migration mechanism should be used if `W0201` becomes a
  hard error by default?

## Resolution

Pending.