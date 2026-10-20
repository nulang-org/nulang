/-
  Known counterexamples in the *current* effect-annotated typing judgment.

  These are positive derivations of wrongly empty effect rows. They deliberately
  record open soundness gaps so a future HasTypeEff redesign has precise
  regression cases to invert (the fixed judgment should reject both).
  No claim about the production compiler is made by this file.
-/

import effects

namespace Nulang

/--
  The current function type Ty.fn does not carry a latent effect row.
  A lambda body may perform IO, yet tLambda gives the closure an empty
  immediate row and tApp ignores effects of invoking the closure.
-/
example : HasTypeEff Context.empty
    (.app (.lambda "x" .int (.perform .IO (.litInt 0))) (.litInt 1))
    .int EffectRow.empty := by
  have hBody : HasTypeEff
      [("x", ⟨[], Ty.int⟩)]
      (.perform .IO (.litInt 0)) .int (EffectRow.singleton .IO) :=
    HasTypeEff.tPerform HasTypeEff.tLitInt
  have hLambda : HasTypeEff Context.empty
      (.lambda "x" .int (.perform .IO (.litInt 0)))
      (.fn .int .int) EffectRow.empty :=
    HasTypeEff.tLambda hBody
  have hArg : HasTypeEff Context.empty
      (.litInt 1) .int EffectRow.empty := HasTypeEff.tLitInt
  have hApp := HasTypeEff.tApp hLambda hArg
  simpa [EffectRow.union, EffectRow.empty] using hApp

/--
  The current tHandle constructor has no typing premise for the handler body,
  so one can attach an IO handler body that performs Net yet obtain an empty
  result effect row. A sound handler rule must account for handler-body effects
  as well as the effects of the handled expression.
-/
example : HasTypeEff Context.empty
    (.handle (.perform .IO (.litInt 0))
      .IO (.perform .Net (.litInt 1)))
    .int EffectRow.empty := by
  have hPerform : HasTypeEff Context.empty
      (.perform .IO (.litInt 0))
      .int (EffectRow.singleton .IO) :=
    HasTypeEff.tPerform HasTypeEff.tLitInt
  have hExpected : HasTypeEff Context.empty
      (.perform .IO (.litInt 0)) .int
      (EffectRow.union (EffectRow.singleton .IO) EffectRow.empty) := by
    simpa [EffectRow.union, EffectRow.empty, EffectRow.singleton] using hPerform
  exact HasTypeEff.tHandle hExpected

end Nulang
