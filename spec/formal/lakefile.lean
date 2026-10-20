import Lake
open Lake DSL

package «nulang-formal» {}

@[default_target]
lean_lib «Nulang» {
  srcDir := "."
  -- The top-level `types`/`capabilities`/`effects` modules are the Core
  -- soundness formalization (types.lean: HM typing + small-step semantics
  -- + the progress/preservation/soundness chain); the `Nulang` root is the
  -- newer type-language/capability-lattice/effect-row formalization.  Both
  -- must be compiled: 89bd0d6 dropped the top-level roots from the default
  -- target, orphaning the soundness proofs from `lake build` (they are back
  -- since 2026-08-14).  The compositional scoped-effect bridge is an\n  -- explicit root to prevent it from silently leaving the build graph.
  roots := #[`Nulang, `types, `capabilities, `effects, `trace_effect_rows, `effect_typing_counterexamples]
}
