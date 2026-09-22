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

  Local handler dispatch safety is machine-checked. Whole-language effect
  progress/preservation remains open until latent function effect rows are
  represented in the formal function type.
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
deriving BEq, DecidableEq, Repr, Inhabited

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
  `dischargedBy handled residual observed` means every statically-known effect
  in `observed` is either the effect handled by the surrounding handler or is
  preserved in the outward `residual` row.

  Open rows are accepted only when their row-variable provenance is preserved;
  an unknown open tail may not be silently discharged by a handler.  This is a
  conservative formal counterpart to the compiler's row-unification rule.
-/
def dischargedBy (handled : EffectLabel) (residual observed : EffectRow) : Prop :=
  match residual, observed with
  | .Closed keep, .Closed seen =>
      ∀ eff, eff ∈ seen → eff = handled ∨ eff ∈ keep
  | .Open keep _, .Closed seen =>
      ∀ eff, eff ∈ seen → eff = handled ∨ eff ∈ keep
  | .Open keep residualRegion, .Open seen observedRegion =>
      residualRegion = observedRegion ∧
      ∀ eff, eff ∈ seen → eff = handled ∨ eff ∈ keep
  | .Closed _, .Open _ _ => False

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
  if ∃ handler ∈ handlers, handler.label = eff then .handled else .unhandled

-- ------------------------------------------------------------------
-- Dispatch safety
-- ------------------------------------------------------------------

/--
  A concrete handler list safely dispatches `eff` whenever the list contains
  at least one matching handler.  This is the local runtime fact used by the
  handler-stack model below; unlike the previous placeholder theorem, the
  conclusion is an actual dispatch property rather than `True`.
-/
theorem effect_safety
  (handlers : List Handler) (eff : EffectLabel)
  (h_present : ∃ handler ∈ handlers, handler.label = eff) :
  dispatch handlers eff = .handled := by
  simp [dispatch, h_present]

/-- The nearest freshly-installed handler always handles its own label. -/
theorem dispatch_fresh_handler
  (handler : Handler) (rest : List Handler) :
  dispatch (handler :: rest) handler.label = .handled := by
  apply effect_safety
  exact ⟨handler, by simp, rfl⟩

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

-- Handle: both the protected computation and the handler body are typed.
-- Every statically-known effect they may perform must either be the handled
-- effect itself or remain visible in the outward residual row.  This closes the
-- previous formal hole where an arbitrary, completely untyped handler body
-- could be attached to a well-typed protected computation.
| tHandle : ∀ {Γ e eff h τ r bodyRow handlerRow},
    HasTypeEff Γ e τ bodyRow →
    HasTypeEff Γ h τ handlerRow →
    EffectRow.dischargedBy eff r bodyRow →
    EffectRow.dischargedBy eff r handlerRow →
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

/--
  Boolean scope test using propositional equality via `decide`, avoiding any
  dependence on a separate `LawfulBEq` assumption for the formal label type.
-/
def contains (hs : HandlerStack) (eff : EffectLabel) : Bool :=
  hs.any (fun active => decide (active = eff))

/-- Runtime dispatch against the active lexical handler stack. -/
def dispatch (hs : HandlerStack) (eff : EffectLabel) : EffectRow.DispatchResult :=
  match contains hs eff with
  | true => EffectRow.DispatchResult.handled
  | false => EffectRow.DispatchResult.unhandled

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
  hs.contains eff = true

-- ==================================================================
-- HANDLER-STACK SAFETY LEMMAS
-- ==================================================================

/-- Installing a handler establishes lexical scope for its own effect. -/
theorem handler_push_establishes_scope
  (hs : HandlerStack) (eff : EffectLabel) :
  HandlerScope (hs.push eff) eff := by
  change (decide (eff = eff) || hs.any (fun active => decide (active = eff))) = true
  simp

/-- Existing handler scopes are preserved when another handler is pushed. -/
theorem handler_push_preserves_scope
  (hs : HandlerStack) (installed eff : EffectLabel)
  (h_scope : HandlerScope hs eff) :
  HandlerScope (hs.push installed) eff := by
  change hs.any (fun active => decide (active = eff)) = true at h_scope
  change
    (decide (installed = eff) || hs.any (fun active => decide (active = eff))) = true
  rw [h_scope]
  simp

/--
  **Local Effect Safety.** Any effect proven to be in the active handler scope
  dispatches to a handler rather than producing an unhandled-effect result.
-/
theorem effect_safety_static
  (hs : HandlerStack) (eff : EffectLabel)
  (h_scope : HandlerScope hs eff) :
  HandlerStack.dispatch hs eff = EffectRow.DispatchResult.handled := by
  change hs.contains eff = true at h_scope
  simp [HandlerStack.dispatch, h_scope]

/--
  Corollary for entering a `handle` scope: the handled effect is immediately
  safe to dispatch while the protected computation/handler body executes.
-/
theorem entered_handler_dispatches
  (hs : HandlerStack) (eff : EffectLabel) :
  HandlerStack.dispatch (hs.push eff) eff = EffectRow.DispatchResult.handled :=
  effect_safety_static (hs.push eff) eff (handler_push_establishes_scope hs eff)

/--
  Inversion for typed handlers: a typed `handle` expression necessarily has a
  typed handler body.  The previous model could not state this property because
  `tHandle` carried no typing premise for the handler body at all.
-/
theorem typed_handle_has_typed_handler
  {Γ : Context} {e h : EffExpr} {eff : EffectLabel} {τ : Ty} {r : EffectRow}
  (typed : HasTypeEff Γ (.handle e eff h) τ r) :
  ∃ handlerRow, HasTypeEff Γ h τ handlerRow := by
  cases typed with
  | tHandle _ handlerTyped _ _ =>
      exact ⟨_, handlerTyped⟩

/-
  The remaining whole-language theorem is intentionally NOT stated as
  `HasTypeEff Γ e τ {} -> no runtime unhandled effect` yet.  The current
  simplified formal `Ty.fn` does not carry latent effect rows, while the Rust
  compiler's function type does.  Proving application safety before modeling
  those latent rows would therefore overclaim.  The next formalization step is
  an effect-aware function type plus progress/preservation over the handler
  stack; these local lemmas are the sound foundation for that proof.
-/

end Nulang
