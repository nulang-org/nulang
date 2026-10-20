/-
  Nulang effect system — row-polymorphic algebraic effects (Koka-inspired).

  Formalizes `EffectRow` (Closed/Open + Region) from `src/effect_checker.rs`
  and `src/types.rs`.  The built-in effect names (`IO`, `Net`, `Spawn`,
  `Send`, `Receive`, `Migrate`, `Async`, `LLM`, `Cost`, `Event`, `FFI`)
  are enumerated; `Provider` and user-defined effects are captured via
  a label type.

  Note: `Provider` was added as a Stable-tier effect 2026-07-19 (RFC 0001,
  item 5 non-breaking phase).  `LLM` is deprecated but still listed for
  backward compatibility.

  A scoped effect safety theorem is proved below; full expression soundness remains open.
-/

import types
namespace Nulang

-- ------------------------------------------------------------------
-- Effect labels
-- ------------------------------------------------------------------

/--
  The built-in effect names.  Mirrors `Effect` enum in `src/types.rs`
  plus `Provider` (added 2026-07-19).  Open (user-defined) effects are
  modelled as arbitrary names via the `UserDefined` variant.
-/
inductive EffectLabel where
| IO        | Net       | FS        | Rand      | Time
| Spawn     | Send      | Receive   | Migrate   | Async
| LLM       | Cost      | Event     | FFI
| Provider
| UserDefined : String → EffectLabel
deriving BEq, Repr, Inhabited

-- ------------------------------------------------------------------
-- Row variables (regions)
-- ------------------------------------------------------------------

/--
  A region is a fresh unification variable used in open rows.
  Mirrors the `Region` type in `src/types.rs`.  Regions are
  compared by equality (not name).
-/
structure Region where
  id : Nat
deriving BEq, Hashable, Inhabited, Repr

-- ------------------------------------------------------------------
-- Effect rows
-- ------------------------------------------------------------------

/--
  An effect row is either closed (a fixed set of labels) or open
  (a set of labels plus a row variable that can be further extended).
  Mirrors `EffectRow` in `src/types.rs`.

  ```
  EffectRow ::= Closed [EffectLabel]
             |  Open   [EffectLabel] Region
  ```
-/
inductive EffectRow where
| Closed : List EffectLabel → EffectRow
| Open   : List EffectLabel → Region → EffectRow
deriving BEq, Repr, Inhabited

namespace EffectRow

-- ------------------------------------------------------------------
-- Row operations
-- ------------------------------------------------------------------

/--
  The empty effect row (no effects performed).  This is `{}` in
  surface syntax: the pure computation row.
-/
def empty : EffectRow := .Closed []

/--
  Singleton row: `{eff}`.
-/
def singleton (eff : EffectLabel) : EffectRow := .Closed [eff]

/--
  Row union: combine the labels of two rows.  For closed rows,
  this is set union.  For open rows, the regions must be unified
  — the row variables collapse to the same region, and labels
  from both sources combine.
-/
def union (r₁ r₂ : EffectRow) : EffectRow :=
  match r₁, r₂ with
  | .Closed a, .Closed b => .Closed (a ++ b)
  | .Open a r, .Closed b => .Open (a ++ b) r
  | .Closed a, .Open b r => .Open (a ++ b) r
  | .Open a r, .Open b _ => .Open (a ++ b) r  -- unification deferred (see note)
  -- ^ NOTE: Open+Open should unify the regions and merge.
  -- Lehel's `scoped labels` approach is the target; this
  -- simplification defers unification to the checker.

/--
  Check whether `eff` is a member of row `r`.
  For closed rows: direct set membership.  For open rows:
  membership in the fixed labels OR the row variable may be
  further instantiated to contain `eff`.
-/
def mem (eff : EffectLabel) (r : EffectRow) : Bool :=
  match r with
  | .Closed ls => ls.contains eff
  | .Open ls _ => ls.contains eff  -- open: the variable may carry `eff`; conservative: false

-- ------------------------------------------------------------------
-- Free regions
-- ------------------------------------------------------------------

/-- Collect the set of regions referenced in `r`. -/
def fv : EffectRow → List Region
| .Closed _     => []
| .Open _ r     => [r]

-- ------------------------------------------------------------------
-- Handler dispatch model
-- ------------------------------------------------------------------

/--
  A handler table maps effect labels to handler code.
  Mirrors `HandlerTable` in `src/bytecode.rs`.

  The formal model abstracts over the actual bytecode offsets:
  a handler is a binding `(label, op_handler)`.
-/
structure Handler where
  label : EffectLabel
  -- handler body (abstracted)

/--
  Dispatch `eff` through a handler stack: find the nearest
  handler matching `eff` and invoke it.  If no handler matches,
  the effect is unhandled (runtime error).
-/
inductive DispatchResult where
| handled   : DispatchResult
| unhandled : DispatchResult
deriving BEq, Repr

def dispatch (handlers : List Handler) (eff : EffectLabel) : DispatchResult :=
  if handlers.any (·.label == eff) then .handled else .unhandled

-- ------------------------------------------------------------------
-- The operational effect-safety theorem lives after HandlerStack, where
-- the model can account for handler scope.  A row-only assertion here
-- previously returned True and established no safety property.

end EffectRow

-- ==================================================================
-- EFFECTFUL EXPRESSION LANGUAGE
-- ==================================================================

/--
  Effectful expressions extend the Core expression language (see
  `spec/formal/types.lean` for `Expr`, `Ty`, `Context`) with effect
  operations: `perform` invokes an effect, `handle` scopes a handler.
-/
inductive EffExpr where
| litInt     : Int → EffExpr
| litBool    : Bool → EffExpr
| litString  : String → EffExpr
| var        : Name → EffExpr
| lambda     : Name → Ty → EffExpr → EffExpr
| app        : EffExpr → EffExpr → EffExpr
| letIn      : Name → EffExpr → EffExpr → EffExpr
| ifThenElse : EffExpr → EffExpr → EffExpr → EffExpr
| unitVal    : EffExpr
| perform    : EffectLabel → EffExpr → EffExpr
| handle     : EffExpr → EffectLabel → EffExpr → EffExpr
deriving Repr, Inhabited

-- ==================================================================
-- EFFECT-ANNOTATED TYPING JUDGMENT  Δ ⊢ e : τ ! r
-- ==================================================================

/--
  The effect-annotated typing judgment for Nulang.

  `HasTypeEff Γ e τ r` means "in context `Γ`, expression `e` has type `τ`
  and may perform effects described by row `r`."

  Rules:

  - `Var` / `Lit*` / `Unit`: pure terms — effect row is empty.
  - `Lambda`: body effects are *latent*; lambda creation is pure.
  - `App`: effects of function and argument combine via row union.
  - `Let`: effects of bound expression and body combine.
  - `If`: effects of guard and both branches combine.
  - `Perform`: performing an effect adds its label to the row.
  - `Handle`: handling removes the effect label from the row.

  Dependencies (from `spec/formal/types.lean`):
  `Context` (`List (Name × Scheme)`), `Scheme.generalize`,
  `Scheme.instantiate`, `defaultFresh`, `Context.freeTypeVars`.
-/
inductive HasTypeEff : Context → EffExpr → Ty → EffectRow → Prop where

-- Pure rules: variables and literals have no effects.
| tVar : ∀ {Γ x τ σ},
    Context.lookup Γ x = some σ →
    (σ.instantiate defaultFresh).1 = τ →
    HasTypeEff Γ (.var x) τ EffectRow.empty

| tLitInt : ∀ {Γ n},
    HasTypeEff Γ (.litInt n) .int EffectRow.empty

| tLitBool : ∀ {Γ b},
    HasTypeEff Γ (.litBool b) .bool EffectRow.empty

| tLitString : ∀ {Γ s},
    HasTypeEff Γ (.litString s) .string EffectRow.empty

| tUnit : ∀ {Γ},
    HasTypeEff Γ .unitVal .unit EffectRow.empty

-- Lambda: the body may have effects, but creating the closure is pure.
| tLambda : ∀ {Γ x τ₁ e τ₂ r},
    HasTypeEff ((x, ⟨[], τ₁⟩) :: Γ) e τ₂ r →
    HasTypeEff Γ (.lambda x τ₁ e) (.fn τ₁ τ₂) EffectRow.empty

-- Application: effect rows of function and argument are combined.
| tApp : ∀ {Γ e₁ e₂ τ₁ τ₂ r₁ r₂},
    HasTypeEff Γ e₁ (.fn τ₂ τ₁) r₁ →
    HasTypeEff Γ e₂ τ₂ r₂ →
    HasTypeEff Γ (.app e₁ e₂) τ₁ (EffectRow.union r₁ r₂)

-- Let: generalize the bound expression's type, combine effect rows.
| tLet : ∀ {Γ x e₁ e₂ τ₁ τ₂ r₁ r₂},
    HasTypeEff Γ e₁ τ₁ r₁ →
    HasTypeEff ((x, Scheme.generalize (Context.freeTypeVars Γ) τ₁) :: Γ) e₂ τ₂ r₂ →
    HasTypeEff Γ (.letIn x e₁ e₂) τ₂ (EffectRow.union r₁ r₂)

-- If: effect rows of all three sub-expressions are combined.
| tIf : ∀ {Γ e₁ e₂ e₃ τ r₁ r₂ r₃},
    HasTypeEff Γ e₁ .bool r₁ →
    HasTypeEff Γ e₂ τ r₂ →
    HasTypeEff Γ e₃ τ r₃ →
    HasTypeEff Γ (.ifThenElse e₁ e₂ e₃) τ
      (EffectRow.union r₁ (EffectRow.union r₂ r₃))

-- Perform: the effect label is added to the row.
-- The argument expression must be pure (no further effects).
| tPerform : ∀ {Γ eff e τ},
    HasTypeEff Γ e τ EffectRow.empty →
    HasTypeEff Γ (.perform eff e) τ (EffectRow.singleton eff)

-- Handle: the handled effect is removed from the row.
-- The handler body `h` is assumed well-formed (its typing is orthogonal).
| tHandle : ∀ {Γ e eff h τ r},
    HasTypeEff Γ e τ (EffectRow.union (EffectRow.singleton eff) r) →
    HasTypeEff Γ (.handle e eff h) τ r

-- ==================================================================
-- HANDLER STACK SEMANTICS
-- ==================================================================

/--
  A handler stack tracks which effect labels are currently being
  handled.  The innermost handler is at the head of the list.
-/
abbrev HandlerStack := List EffectLabel

namespace HandlerStack

/-- Push an effect label onto the stack (entering a `handle` scope). -/
def push (hs : HandlerStack) (eff : EffectLabel) : HandlerStack :=
  eff :: hs

/-- Pop an effect label from the stack (exiting a `handle` scope). -/
def pop (hs : HandlerStack) (eff : EffectLabel) : HandlerStack :=
  hs.erase eff

/-- The empty handler stack — no effects are currently handled. -/
def empty : HandlerStack := []

end HandlerStack

-- ------------------------------------------------------------------
-- Handler stack transitions (Handle pushes, Unwind pops)
-- ------------------------------------------------------------------

/--
  Handler stack transition relation.

  - `push`: entering a `handle` scope pushes the effect label.
  - `pop`:  completing (unwinding) a `handle` scope pops the label.

  These model the runtime dynamics of the handler stack during
  evaluation of effectful programs.
-/
inductive HandlerTrans : HandlerStack → HandlerStack → Prop where
| push : ∀ {hs eff}, HandlerTrans hs (hs.push eff)
| pop  : ∀ {hs eff}, HandlerTrans (hs.push eff) hs

-- ==================================================================
-- HANDLER SCOPE PREDICATE
-- ==================================================================

/--
  `HandlerScope hs eff` holds when effect `eff` is bound (has an
  active handler) in handler stack `hs` — i.e., `eff` appears in
  the stack, meaning some enclosing `handle` scope covers it.

  Combined with `HandlerTrans`, this models:
  - `Handle` pushes `eff` onto the stack (entering scope).
  - `Unwind` pops `eff` from the stack (exiting scope).
-/
def HandlerScope (hs : HandlerStack) (eff : EffectLabel) : Prop :=
  eff ∈ hs

-- ==================================================================
-- SCOPED EFFECT EXECUTION MODEL
-- ==================================================================

/-
  This deliberately small execution trace captures the control structures
  needed to reason about dynamically scoped effect handlers: performing an
  operation, sequencing computations, and entering/leaving a handler scope.

  It does NOT yet model closures, continuations, resumptions, handler-body
  effects, or the complete HasTypeEff judgment.  In particular HasTypeEff
  currently loses latent effects at function application.  Consequently
  these theorems must not be represented as soundness of every Nulang term.
-/
inductive EffectTrace where
| done    : EffectTrace
| perform : EffectLabel → EffectTrace
| seq     : EffectTrace → EffectTrace → EffectTrace
| handle  : EffectLabel → EffectTrace → EffectTrace
deriving Repr

/--
  An effect escapes a trace when no *lexically enclosing* handler in that
  trace intercepts it.  This describes the residual static effect set, as
  a proposition, without trusting a concrete row-unification algorithm.
-/
inductive Escapes : EffectTrace → EffectLabel → Prop where
| perform (eff : EffectLabel) :
    Escapes (.perform eff) eff
| seqLeft {left right eff} :
    Escapes left eff → Escapes (.seq left right) eff
| seqRight {left right eff} :
    Escapes right eff → Escapes (.seq left right) eff
| handle {handler body eff} :
    Escapes body eff → eff ≠ handler →
    Escapes (.handle handler body) eff

/--
  A perform reaches runtime without any matching active handler.
  Entering a handler evaluates its body with the handler pushed; returning
  from the body restores the caller's stack (the pop is implicit in scope).
  Sequencing propagates a possible unhandled operation on either side.
-/
inductive Unhandled : HandlerStack → EffectTrace → EffectLabel → Prop where
| perform {hs eff} :
    eff ∉ hs → Unhandled hs (.perform eff) eff
| seqLeft {hs left right eff} :
    Unhandled hs left eff → Unhandled hs (.seq left right) eff
| seqRight {hs left right eff} :
    Unhandled hs right eff → Unhandled hs (.seq left right) eff
| handle {hs handler body eff} :
    Unhandled (handler :: hs) body eff →
    Unhandled hs (.handle handler body) eff

/--
  Soundness bridge: every dynamically unhandled operation escapes the
  lexically scoped handlers *and* is absent from the caller's handler stack.
  Proved by induction over runtime unhandled-operation derivations.
-/
theorem unhandled_implies_escape {hs : HandlerStack}
    {trace : EffectTrace} {eff : EffectLabel}
    (h : Unhandled hs trace eff) :
    Escapes trace eff ∧ eff ∉ hs := by
  induction h with
  | perform hnot =>
      exact ⟨.perform _, hnot⟩
  | seqLeft _ ih =>
      exact ⟨.seqLeft ih.1, ih.2⟩
  | seqRight _ ih =>
      exact ⟨.seqRight ih.1, ih.2⟩
  | handle _ ih =>
      refine ⟨.handle ih.1 ?_, ?_⟩
      · intro heq
        apply ih.2
        simp [heq]
      · intro hmem
        apply ih.2
        simp [hmem]

/--
  Scoped handler safety: if every statically escaping effect is covered by
  the incoming handler stack, no operation can be unhandled at runtime.
-/
theorem effect_safety (hs : HandlerStack) (trace : EffectTrace)
    (hcovered : ∀ eff, Escapes trace eff → eff ∈ hs) :
    ∀ eff, ¬ Unhandled hs trace eff := by
  intro eff hbad
  have ⟨hescapes, hmissing⟩ := unhandled_implies_escape hbad
  exact hmissing (hcovered eff hescapes)

/--
  Empty residual-effect safety: a trace with no escaping operations needs
  no external handler.  Internal performed operations may still exist, but
  each must be handled by its own lexical scope.
-/
theorem effect_safety_static (trace : EffectTrace)
    (hclosed : ∀ eff, ¬ Escapes trace eff) :
    ∀ eff, ¬ Unhandled HandlerStack.empty trace eff := by
  intro eff hbad
  exact hclosed eff (unhandled_implies_escape hbad).1

-- Regression witnesses: the property is not True by construction.
-- An unhandled perform is reachable, while a locally handled perform is not.
example : Unhandled HandlerStack.empty (.perform .IO) .IO :=
  .perform (by simp [HandlerStack.empty])

example : Escapes (.perform .IO) .IO := .perform .IO

example : ¬ Unhandled HandlerStack.empty (.handle .IO (.perform .IO)) .IO := by
  have hclosed : ∀ eff, ¬ Escapes (.handle .IO (.perform .IO)) eff := by
    intro eff h
    cases h with
    | handle inner hneq =>
        cases inner
        exact hneq rfl
  exact effect_safety_static _ hclosed .IO

end Nulang
