/-
  Regression witnesses for two previously unsound empty-effect derivations.

  Before effect-aware function types and typed handler bodies, the same
  expressions admitted HasTypeEff judgments with an empty effect row.
  The updated typing rules account for IO and Net instead.

  This formal fragment is monomorphic and does not yet model higher-order
  polymorphic effect rows, captured environments, or continuation resumptions.
-/

import effects

namespace Nulang

/--
  Calling an IO-performing lambda must include its *latent* IO effect.
  Lambda creation itself remains pure.
-/
example : HasTypeEff EffContext.empty
    (.app (.lambda "x" .int (.perform .IO (.litInt 0))) (.litInt 1))
    (EffTy.base .int) (EffectRow.singleton .IO) := by
  have hBody : HasTypeEff
      [("x", EffTy.base Ty.int)]
      (.perform .IO (.litInt 0)) (EffTy.base .int) (EffectRow.singleton .IO) :=
    HasTypeEff.tPerform HasTypeEff.tLitInt
  have hLambda : HasTypeEff EffContext.empty
      (.lambda "x" .int (.perform .IO (.litInt 0)))
      (EffTy.arrow (EffTy.base .int) (EffTy.base .int) (EffectRow.singleton .IO))
      EffectRow.empty :=
    HasTypeEff.tLambda hBody
  have hArg : HasTypeEff EffContext.empty
      (.litInt 1) (EffTy.base .int) EffectRow.empty := HasTypeEff.tLitInt
  have hApp := HasTypeEff.tApp hLambda hArg
  simpa [EffectRow.union, EffectRow.empty, EffectRow.singleton] using hApp

/--
  Handling IO does not erase the Net effect introduced by the handler body.
-/
example : HasTypeEff EffContext.empty
    (.handle (.perform .IO (.litInt 0))
      .IO (.perform .Net (.litInt 1)))
    (EffTy.base .int) (EffectRow.singleton .Net) := by
  have hPerform : HasTypeEff EffContext.empty
      (.perform .IO (.litInt 0))
      (EffTy.base .int) (EffectRow.singleton .IO) :=
    HasTypeEff.tPerform HasTypeEff.tLitInt
  have hHandler : HasTypeEff EffContext.empty
      (.perform .Net (.litInt 1))
      (EffTy.base .int) (EffectRow.singleton .Net) :=
    HasTypeEff.tPerform HasTypeEff.tLitInt
  have hHandled := HasTypeEff.tHandle hPerform hHandler
  simpa [EffectRow.union, EffectRow.without, EffectRow.empty,
    EffectRow.singleton, List.filter] using hHandled

end Nulang
