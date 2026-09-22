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
-- EFFECT-AWARE TYPES AND EXPRESSIONS
-- ==================================================================

/--
  Effect-aware type used by the effect calculus.

  The pure HM proof in `types.lean` intentionally keeps its smaller `Ty`.
  This wrapper is the effect layer's function type: a function carries the
  latent effect row produced when the function body is invoked.  Keeping this
  separate avoids introducing a circular dependency from the already-proved HM
  core back into `EffectRow`.
-/
inductive EffTy where
| base : Ty → EffTy
| fn   : EffTy → EffTy → EffectRow → EffTy
deriving Repr, Inhabited

namespace EffTy

def int : EffTy := .base Ty.int
def bool : EffTy := .base Ty.bool
def string : EffTy := .base Ty.string
def unit : EffTy := .base Ty.unit

end EffTy

/-- Monomorphic context for the effect-safety calculus. -/
abbrev EffContext := List (Name × EffTy)

/-- Nearest-binding lookup for effect-aware types. -/
def EffContext.lookup (Γ : EffContext) (x : Name) : Option EffTy :=
  match Γ with
  | [] => none
  | (y, τ) :: rest => if x == y then some τ else rest.lookup x

/--
  Effectful expressions extend the Core expression language with `perform` and
  `handle`. Lambda annotations use `EffTy` so higher-order function
  parameters can themselves carry latent effect contracts.
-/
inductive EffExpr where
| litInt     : Int → EffExpr
| litBool    : Bool → EffExpr
| litString  : String → EffExpr
| var        : Name → EffExpr
| lambda     : Name → EffTy → EffExpr → EffExpr
| app        : EffExpr → EffExpr → EffExpr
| letIn      : Name → EffExpr → EffExpr → EffExpr
| ifThenElse : EffExpr → EffExpr → EffExpr → EffExpr
| unitVal    : EffExpr
| perform    : EffectLabel → EffExpr → EffExpr
| handle     : EffExpr → EffectLabel → EffExpr → EffExpr
deriving Repr, Inhabited

-- ==================================================================
-- EFFECT-ANNOTATED TYPING JUDGMENT  Γ ⊢ e : τ ! r
-- ==================================================================

/--
  `HasTypeEff Γ e τ r` means expression `e` has effect-aware type `τ` and
  evaluating it may perform effects described by row `r`.

  The critical higher-order rule is `tApp`: evaluating a function expression
  and its argument contributes their immediate rows, and invoking the function
  additionally contributes the latent row stored in `EffTy.fn`.

  This formal effect layer is deliberately monomorphic. HM generalization and
  substitution are proved separately in `types.lean`; the eventual combined
  proof must connect those results rather than hiding latent effects inside the
  pure `Ty.fn`.
-/
inductive HasTypeEff : EffContext → EffExpr → EffTy → EffectRow → Prop where

-- Variable lookup is pure; a function variable retains latent effects in EffTy.
| tVar : ∀ {Γ x τ},
    EffContext.lookup Γ x = some τ →
    HasTypeEff Γ (.var x) τ EffectRow.empty

| tLitInt : ∀ {Γ n},
    HasTypeEff Γ (.litInt n) .int EffectRow.empty

| tLitBool : ∀ {Γ b},
    HasTypeEff Γ (.litBool b) .bool EffectRow.empty

| tLitString : ∀ {Γ s},
    HasTypeEff Γ (.litString s) .string EffectRow.empty

| tUnit : ∀ {Γ},
    HasTypeEff Γ .unitVal .unit EffectRow.empty

-- Lambda creation is pure; body effects are stored as a latent function row.
| tLambda : ∀ {Γ x τ₁ e τ₂ latent},
    HasTypeEff ((x, τ₁) :: Γ) e τ₂ latent →
    HasTypeEff Γ (.lambda x τ₁ e) (.fn τ₁ τ₂ latent) EffectRow.empty

-- Application exposes the latent row in addition to evaluation-time effects.
| tApp : ∀ {Γ e₁ e₂ τ₁ τ₂ immediateFn immediateArg latent},
    HasTypeEff Γ e₁ (.fn τ₂ τ₁ latent) immediateFn →
    HasTypeEff Γ e₂ τ₂ immediateArg →
    HasTypeEff Γ (.app e₁ e₂) τ₁
      (EffectRow.union immediateFn (EffectRow.union immediateArg latent))

-- Let preserves the effect-aware type, including any latent function row.
| tLet : ∀ {Γ x e₁ e₂ τ₁ τ₂ r₁ r₂},
    HasTypeEff Γ e₁ τ₁ r₁ →
    HasTypeEff ((x, τ₁) :: Γ) e₂ τ₂ r₂ →
    HasTypeEff Γ (.letIn x e₁ e₂) τ₂ (EffectRow.union r₁ r₂)

-- If: effect rows of guard and both branches combine.
| tIf : ∀ {Γ e₁ e₂ e₃ τ r₁ r₂ r₃},
    HasTypeEff Γ e₁ .bool r₁ →
    HasTypeEff Γ e₂ τ r₂ →
    HasTypeEff Γ e₃ τ r₃ →
    HasTypeEff Γ (.ifThenElse e₁ e₂ e₃) τ
      (EffectRow.union r₁ (EffectRow.union r₂ r₃))

-- Perform: the effect label is added to the row.
-- The simplified effect signature model keeps argument/result type aligned.
| tPerform : ∀ {Γ eff e τ},
    HasTypeEff Γ e τ EffectRow.empty →
    HasTypeEff Γ (.perform eff e) τ (EffectRow.singleton eff)

-- Handle: both protected computation and handler body are typed, and no
-- statically-known effect can disappear unless this handler discharges it.
| tHandle : ∀ {Γ e eff h τ r bodyRow handlerRow},
    HasTypeEff Γ e τ bodyRow →
    HasTypeEff Γ h τ handlerRow →
    EffectRow.dischargedBy eff r bodyRow →
    EffectRow.dischargedBy eff r handlerRow →
    HasTypeEff Γ (.handle e eff h) τ r

/--
  Inversion for application: the result row explicitly includes the latent row
  carried by the function type. This is the property the previous `Ty.fn`
  formalization could not state.
-/
theorem typed_application_accounts_for_latent_effects
  {Γ : EffContext} {e₁ e₂ : EffExpr} {τ : EffTy} {r : EffectRow}
  (typed : HasTypeEff Γ (.app e₁ e₂) τ r) :
  ∃ argTy immediateFn immediateArg latent,
    HasTypeEff Γ e₁ (.fn argTy τ latent) immediateFn ∧
    HasTypeEff Γ e₂ argTy immediateArg ∧
    r = EffectRow.union immediateFn (EffectRow.union immediateArg latent) := by
  cases typed with
  | tApp fnTyped argTyped =>
      exact ⟨_, _, _, _, fnTyped, argTyped, rfl⟩

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

namespace EffectRow

/--
  A row is accounted for by a handler stack when it is closed and every effect
  named by the row is currently in lexical handler scope.

  Open rows are deliberately not considered fully accounted: the unknown tail
  may later instantiate to an effect for which no handler exists.
-/
def accountedBy (hs : HandlerStack) : EffectRow → Prop
| .Closed effects => ∀ eff, eff ∈ effects → HandlerScope hs eff
| .Open _ _ => False

/-- A singleton effect row is accounted exactly when its effect is in scope. -/
theorem accountedBy_singleton_iff
  (hs : HandlerStack) (eff : EffectLabel) :
  accountedBy hs (singleton eff) ↔ HandlerScope hs eff := by
  simp [accountedBy, singleton]

/--
  Handler-stack accounting distributes over effect-row union.

  This is important for application/let/branch rules: if the combined outward
  row is accounted, each constituent obligation is accounted independently.
-/
theorem accountedBy_union_iff
  (hs : HandlerStack) (left right : EffectRow) :
  accountedBy hs (union left right) ↔
    accountedBy hs left ∧ accountedBy hs right := by
  cases left <;> cases right <;> simp [accountedBy, union]

end EffectRow

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

namespace EffectRow

/--
  If a closed outward row is accounted by the current handler stack, then a
  row accepted by `dischargedBy handled outward observed` is accounted after
  pushing `handled`.

  This is the static-to-runtime bridge used by handler safety: an effect may
  disappear from the outward row only because the lexical handler being
  entered accounts for it.
-/
theorem dischargedBy_preserves_accounting
  (hs : HandlerStack) (handled : EffectLabel)
  (residual observed : EffectRow)
  (h_discharged : dischargedBy handled residual observed)
  (h_residual : accountedBy hs residual) :
  accountedBy (hs.push handled) observed := by
  cases residual with
  | Open keep region =>
      exact False.elim h_residual
  | Closed keep =>
      cases observed with
      | Open seen region =>
          exact False.elim h_discharged
      | Closed seen =>
          intro eff h_seen
          have h_account := h_discharged eff h_seen
          cases h_account with
          | inl h_handled =>
              subst eff
              exact handler_push_establishes_scope hs handled
          | inr h_keep =>
              exact handler_push_preserves_scope
                hs handled eff (h_residual eff h_keep)

end EffectRow

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
  {Γ : EffContext} {e h : EffExpr} {eff : EffectLabel} {τ : EffTy} {r : EffectRow}
  (typed : HasTypeEff Γ (.handle e eff h) τ r) :
  ∃ handlerRow, HasTypeEff Γ h τ handlerRow := by
  cases typed with
  | tHandle _ handlerTyped _ _ =>
      exact ⟨_, handlerTyped⟩

/--
  A well-typed handler whose outward row is already accounted by the current
  stack has both its protected computation and handler body accounted after the
  handled effect is pushed.

  This is the key induction lemma needed by a future progress theorem over
  expression + handler-stack machine state.
-/
theorem typed_handle_rows_accounted_after_push
  {Γ : EffContext} {e h : EffExpr} {eff : EffectLabel}
  {τ : EffTy} {residual : EffectRow}
  (typed : HasTypeEff Γ (.handle e eff h) τ residual)
  (hs : HandlerStack)
  (h_residual : EffectRow.accountedBy hs residual) :
  ∃ bodyRow handlerRow,
    HasTypeEff Γ e τ bodyRow ∧
    HasTypeEff Γ h τ handlerRow ∧
    EffectRow.accountedBy (hs.push eff) bodyRow ∧
    EffectRow.accountedBy (hs.push eff) handlerRow := by
  cases typed with
  | tHandle bodyTyped handlerTyped bodyDischarged handlerDischarged =>
      exact ⟨_, _, bodyTyped, handlerTyped,
        EffectRow.dischargedBy_preserves_accounting
          hs eff _ _ bodyDischarged h_residual,
        EffectRow.dischargedBy_preserves_accounting
          hs eff _ _ handlerDischarged h_residual⟩

/--
  An accounted application accounts for all three effect sources separately:
  evaluating the function expression, evaluating the argument, and invoking the
  callee's latent effect contract.
-/
theorem typed_application_effect_rows_accounted
  {Γ : EffContext} {e₁ e₂ : EffExpr} {τ : EffTy} {row : EffectRow}
  (typed : HasTypeEff Γ (.app e₁ e₂) τ row)
  (hs : HandlerStack)
  (h_row : EffectRow.accountedBy hs row) :
  ∃ argTy immediateFn immediateArg latent,
    HasTypeEff Γ e₁ (.fn argTy τ latent) immediateFn ∧
    HasTypeEff Γ e₂ argTy immediateArg ∧
    EffectRow.accountedBy hs immediateFn ∧
    EffectRow.accountedBy hs immediateArg ∧
    EffectRow.accountedBy hs latent := by
  rcases typed_application_accounts_for_latent_effects typed with
    ⟨argTy, immediateFn, immediateArg, latent, fnTyped, argTyped, rfl⟩
  have h_outer :=
    (EffectRow.accountedBy_union_iff
      hs immediateFn (EffectRow.union immediateArg latent)).mp h_row
  have h_inner :=
    (EffectRow.accountedBy_union_iff hs immediateArg latent).mp h_outer.2
  exact ⟨argTy, immediateFn, immediateArg, latent,
    fnTyped, argTyped, h_outer.1, h_inner.1, h_inner.2⟩

/--
  A typed direct `perform` whose outward row is accounted by the active stack
  cannot dispatch as unhandled.
-/
theorem typed_perform_dispatches_when_accounted
  {Γ : EffContext} {argument : EffExpr} {eff : EffectLabel}
  {τ : EffTy} {row : EffectRow}
  (typed : HasTypeEff Γ (.perform eff argument) τ row)
  (hs : HandlerStack)
  (h_row : EffectRow.accountedBy hs row) :
  HandlerStack.dispatch hs eff = EffectRow.DispatchResult.handled := by
  cases typed with
  | tPerform argumentTyped =>
      have h_scope : HandlerScope hs eff :=
        (EffectRow.accountedBy_singleton_iff hs eff).mp h_row
      exact effect_safety_static hs eff h_scope

/-
  The effect calculus now preserves latent function rows through variables and
  application. The remaining whole-language theorem is intentionally not yet
  claimed: it still requires an operational semantics that steps expressions
  together with the handler stack, followed by progress and preservation for
  that combined state.
-/

end Nulang
