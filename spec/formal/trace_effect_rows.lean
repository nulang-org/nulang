/-
  Compositional residual effects for the scoped EffectTrace sublanguage.

  This is a deliberately small bridge between static effect inference and
  the dynamic handler-safety theorem in effects.lean.  StaticEffects is a
  characteristic predicate (a set of EffectLabel), rather than the list-backed
  EffectRow checker representation.  Proofs about that representation,
  latent function effects, resumptions and handler bodies remain open.
-/

import effects

namespace Nulang

/--
  The statically inferred *escaping* effects of a trace.  Effect handlers
  subtract their own label, and sequence combines the residual effects of
  both components.  The result is a predicate representing a set of labels.
-/
def StaticEffects : EffectTrace → EffectLabel → Prop
  | .done, _ => False
  | .perform operation, label => operation = label
  | .seq first second, label =>
      StaticEffects first label ∨ StaticEffects second label
  | .handle handler body, label =>
      StaticEffects body label ∧ label ≠ handler

/-- Every escaping operation is included in the compositional static set. -/
theorem escape_implies_static :
    ∀ (trace : EffectTrace) (label : EffectLabel),
      Escapes trace label → StaticEffects trace label := by
  intro trace
  induction trace with
  | done =>
      intro label h
      cases h
  | perform operation =>
      intro label h
      cases h
      rfl
  | seq first second ihFirst ihSecond =>
      intro label h
      cases h with
      | seqLeft hFirst =>
          exact Or.inl (ihFirst label hFirst)
      | seqRight hSecond =>
          exact Or.inr (ihSecond label hSecond)
  | handle handler body ih =>
      intro label h
      cases h with
      | handle hBody hDifferent =>
          exact ⟨ih label hBody, hDifferent⟩

/-- Every label inferred by the static set can actually escape its trace. -/
theorem static_implies_escape :
    ∀ (trace : EffectTrace) (label : EffectLabel),
      StaticEffects trace label → Escapes trace label := by
  intro trace
  induction trace with
  | done =>
      intro label h
      exact False.elim h
  | perform operation =>
      intro label h
      cases h
      exact Escapes.perform operation
  | seq first second ihFirst ihSecond =>
      intro label h
      cases h with
      | inl hFirst =>
          exact Escapes.seqLeft (ihFirst label hFirst)
      | inr hSecond =>
          exact Escapes.seqRight (ihSecond label hSecond)
  | handle handler body ih =>
      intro label h
      exact Escapes.handle (ih label h.1) h.2

/--
  Static trace inference exactly describes the escaping-effect relation.
  This equivalence is stronger than an assertion that a chosen effect row
  happens to be nonempty.
-/
theorem static_effects_exact (trace : EffectTrace) (label : EffectLabel) :
    StaticEffects trace label ↔ Escapes trace label := by
  constructor
  · exact static_implies_escape trace label
  · exact escape_implies_static trace label

/--
  A dynamically unhandled operation must belong to the compositional
  residual-effect set, and cannot already be in the caller's handler stack.
-/
theorem unhandled_implies_static {hs : HandlerStack}
    {trace : EffectTrace} {label : EffectLabel}
    (h : Unhandled hs trace label) :
    StaticEffects trace label ∧ label ∉ hs := by
  obtain ⟨hEscapes, hMissing⟩ := unhandled_implies_escape h
  exact ⟨escape_implies_static trace label hEscapes, hMissing⟩

/--
  If the incoming handler stack covers all statically inferred residual
  effects, every operation in the trace has an available handler.
-/
theorem static_effects_safe (hs : HandlerStack) (trace : EffectTrace)
    (hCovered : ∀ label, StaticEffects trace label → label ∈ hs) :
    ∀ label, ¬ Unhandled hs trace label := by
  intro label hUnhandled
  obtain ⟨hStatic, hMissing⟩ := unhandled_implies_static hUnhandled
  exact hMissing (hCovered label hStatic)

/-- A trace with no residual effects can run without external handlers. -/
theorem closed_static_effects_safe (trace : EffectTrace)
    (hClosed : ∀ label, ¬ StaticEffects trace label) :
    ∀ label, ¬ Unhandled HandlerStack.empty trace label := by
  intro label hUnhandled
  exact hClosed label (unhandled_implies_static hUnhandled).1

-- Regression examples: these cover both addition and subtraction of
-- residual effects, and rule out vacuous proofs of trace safety.

example : StaticEffects (.perform .IO) .IO := by
  rfl

example : ¬ StaticEffects (.handle .IO (.perform .IO)) .IO := by
  simp [StaticEffects]

example : StaticEffects
    (.handle .IO (.seq (.perform .IO) (.perform .Net))) .Net := by
  simp [StaticEffects]

example : ¬ StaticEffects
    (.handle .IO (.seq (.perform .IO) (.perform .Net))) .IO := by
  simp [StaticEffects]

example : Unhandled HandlerStack.empty
    (.handle .IO (.perform .Net)) .Net := by
  apply Unhandled.handle
  apply Unhandled.perform
  simp [HandlerStack.empty]

end Nulang
