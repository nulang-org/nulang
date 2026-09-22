# Nulang Formal Semantics

> Machine-checked formal specification of the Nulang type system,
> capability lattice, and algebraic effects in Lean 4.
>
> **Status:** The Core soundness chain — `progress`, `preservation`,
> `type_soundness` — is machine-checked (2026-08-14). The capability
> lattice laws (`join` assoc/comm/idem), `cap_sendable`, and
> `discharge_sendable` are proved. The linear checker now has a formal
> split-flow model whose `may`/union and `must`/intersection branch laws
> are proved in `capabilities.lean`; the former single-context
> `linear_at_most_once` conjecture and its `sorry` were removed because
> that statement was known false. `effects.lean` now also has non-vacuous
> proofs for pushed-handler dispatch, rejection of a bare `perform` as pure,
> and affine continuation consumption. The stronger whole-program operational
> effect-safety theorem remains open.

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
| `capabilities.lean` | Capability lattice laws, sendability, split linear-flow branch laws | **Proved for the modeled laws; no `sorry`** |
| `effects.lean` | Effect rows, handler dispatch, static direct-perform safety, affine continuation state | **Local safety laws proved; full operational theorem open** |

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

### Split linear consumption flow — proved branch laws
The capability checker carries two output-flow facts per ownership binding:
`may` means a reaching path has consumed/moved the binding, while `must`
means every reaching path has consumed/moved it. Branch joins union `may`
and intersect `must`. The formal model proves that one-branch consumption
cannot be forgotten (so later reuse is rejected), cannot falsely discharge an
exactly-once obligation, and that consumption on both branches does discharge
that obligation.

This replaces the old `linear_at_most_once` theorem, which was intentionally
left as a `sorry` because it was false under the single-context
`HasTypeCap` judgment. A future full input/output-context typing judgment can
lift these proved flow laws over the complete Core expression relation.

### Effect and continuation safety — local laws proved
`effects.lean` no longer uses vacuous `True` theorems. It proves three
concrete invariants that correspond directly to the implementation:

1. pushing a handler for an effect makes dispatch of that effect resolve as
   handled;
2. a bare `perform` cannot inhabit the empty effect row; and
3. the continuation state machine is affine: the first resume consumes the
   live continuation and a second resume has no transition.

The `tHandle` rule also types the handler body and propagates its own effects
instead of assuming that body is orthogonally well formed. What remains open is
the stronger whole-program theorem connecting arbitrary typed evaluation steps,
handler-stack push/pop dynamics, and absence of runtime unhandled-effect errors.
That proof belongs in the combined operational model rather than being claimed
from these local lemmas.

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
