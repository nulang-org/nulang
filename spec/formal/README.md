# Nulang Formal Semantics

> Machine-checked formal specification of the Nulang type system,
> capability lattice, and algebraic effects in Lean 4.
>
> **Status:** The Core soundness chain — `progress`, `preservation`,
> `type_soundness` — is machine-checked (2026-08-14). The capability
> lattice laws (`join` assoc/comm/idem) and `cap_sendable`/
> `discharge_sendable` are proved. Two items remain open:
> `linear_at_most_once` (requires the split-context refinement of
> `HasTypeCap`, documented in `capabilities.lean`) and complete effect
> safety for the full expression language (latent function effects,
> resumption, and handler-body effects). The scoped trace submodel now proves
> the absence of unhandled effects when residual effects are covered.

## Purpose

Per [GOVERNANCE.md §7](../../GOVERNANCE.md#7-authoritative-artifacts), the
formal model is the authoritative definition of Nulang's semantics. Where
the formal model and prose specification (`SPEC2.md`) disagree, the formal
model takes precedence.

## Structure

Two layers are formalized:

| File | Content | Status |
|---|---|---|
| `Nulang/Types.lean` | Type language, `freeVars`, `Subst`, `occurs`, `mgu` | Formalized |
| `Nulang/Capabilities.lean` | Capability lattice, `subtype`, `join`, `isSendable` | Formalized |
| `Nulang/Effects.lean` | Effect rows, `subrow`, `union` | Formalized |
| `types.lean` | HM `HasType`, small-step semantics, `progress`/`preservation`/`type_soundness` | **Proved** |
| `capabilities.lean` | Capability lattice laws, `cap_sendable`, `discharge_sendable` | Proved (`linear_at_most_once` open) |
| `effects.lean` | Effect rows; scoped `EffectTrace`/`Escapes`/`Unhandled` operational model | Scoped handler-safety proved; full expression soundness open |
| `trace_effect_rows.lean` | Compositional static residual-effect predicate + exactness bridge to `Escapes` | Static/dynamic correspondence (scoped trace only) |

## Theorems

### Type Soundness — proved
```
Theorem type_soundness:
  ∅ ⊢ e : τ ∧ e ↦ v ⇒ ∅ ⊢ v : τ
```
A well-typed closed program either diverges or evaluates to a value of the
same type.  Proved via `progress` (well-typed closed terms are values or
can step) and `preservation` (stepping preserves type under a closed-annotation
invariant `annotationsClosed`, plus the closed-value `substitution_lemma`).
This is the fundamental correctness property of the type system.

### Capability Sendability — proved
```
Theorem cap_sendable:
  isSendable c = true → value tagged with c can cross actor boundaries
  without violating isolation
```
Values with `iso`, `val`, `tag`, `lineariso`, or `linear` capabilities
are safe to send between actors.

### Linear at-most-once — open conjecture
`linear_at_most_once` states that a `LinearIso` binding is consumed after its
single use.  This property is **not** a theorem of the single-context
`HasTypeCap` judgment (it is false there — see the counterexample in
`capabilities.lean`), because that judgment carries no *output* context.  It
requires the split-context refinement `Γ ⊢ e : τ / Γ'`, which is out of scope
for the current formalization and stated as a conjecture per the RFC 0003
Item 2 contingency.

### Scoped Effect Safety — proved for the trace submodel

`effect_safety` establishes that an effect cannot be dynamically unhandled
when every statically escaping effect is present in the incoming handler
stack. `effect_safety_static` is the special case in which no effect escapes
the trace and the initial handler stack is empty.

The model makes **real, non-vacuous propositions** about `Unhandled` and
`Escapes`. It includes perform, sequence, and handler-scope push/pop
(the pop is implicit upon exiting the nested scope). It includes regression
witnesses for both an unhandled `perform IO` and a lexically handled one.

**Important limitation:** this does **not** yet prove
`HasTypeEff Γ e τ EffectRow.empty →` runtime effect safety for all
`EffExpr` terms. `HasTypeEff` does not yet record latent function effects
in function types, and its `tHandle` rule assumes an unconstrained handler
body. The submodel also excludes continuations and resumption. Bridging
this proof to the complete typed operational semantics is still open; the
formal trace theorem must not be presented as production-wide effect
soundness or a guarantee about external side effects.

### Compositional trace-effect inference (scoped model)

`trace_effect_rows.lean` defines `StaticEffects trace label` inductively
by *computation over the trace*: performing an effect adds its label,
sequencing takes the union, and handling one label subtracts that label.
`static_effects_exact` proves this predicate coincides with the independent
`Escapes` relation from `effects.lean`.
`static_effects_safe` then establishes that covering all residual labels
with incoming handlers prevents an unhandled operation. Regression examples
check label subtraction, preservation of unrelated effects, and an unhandled
operation outside the matching handler.

This is a static/dynamic correspondence proof for the **trace fragment**.
It is not a proof that the production `EffectRow.union` implementation,
`HasTypeEff` typing rules, or compiler inference are sound. That connection,
particularly latent function effects and handler-body typing, remains a
separate required validation.

## Build

```bash
cd spec/formal
lake build
```

## Build-graph note (2026-08-14)

`spec/formal/lakefile.lean` roots `#[Nulang, types, capabilities, effects]`.
From 89bd0d6 (2026-08-09) until 2026-08-14 the root list was `#[Nulang]`
only — the top-level `types.lean`/`capabilities.lean`/`effects.lean`
(the Core soundness formalization) were orphaned from `lake build`.
Proofs claimed for those files in commits fe610d8/dd3aafa were never
type-checked and have been reverted; the honest 9-`sorry` state of
2026-08-02 through 2026-08-14 was the 8 Core soundness sorries in
`types.lean` plus `linear_at_most_once` in `capabilities.lean`.  The
soundness chain was proved 2026-08-14, leaving `linear_at_most_once` as the
single remaining `sorry` (CI sorry-ratchet baseline is now 1).

## References

- `src/types.rs` — Rust implementation (oracle)
- `src/typechecker.rs` — Algorithm W implementation
- `src/effect_checker.rs` — Effect + capability checker
- `GOVERNANCE.md` §7 — Authoritative artifacts
- RFC 0003 Item 2 — Formal semantics scoping
