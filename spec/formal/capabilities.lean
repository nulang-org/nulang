/-
  Nulang capability lattice — Pony-inspired reference capabilities.

  Formalizes the eight-capability lattice from `src/types.rs` (Capability enum):
  LinearIso → Iso → Trn → Ref → Box → Tag, with Linear → Val → Box.
  Operations: `join`, `is_subtype_of`, `is_sendable`, `is_isolated`,
  `discharge_linear`.

  Theorem `cap_sendable` is proved below; capability ordering mirrors src/types.rs.
-/

import types
namespace Nulang

-- ------------------------------------------------------------------
-- Capability lattice
-- ------------------------------------------------------------------

/-
  The eight capability constants.  Mirrors `Capability` in `src/types.rs`.
  Lattice: LinearIso -> Iso -> Trn -> Ref -> Box -> Tag,
  Linear -> Val -> Box -> Tag, Iso -> Val, LinearIso -> Linear.
  Subtyping follows the lattice order.
-/
inductive Cap where
| LinearIso | Linear | Iso | Trn | Ref | Val | Box | Tag
deriving BEq, Repr, Inhabited

namespace Cap

-- ------------------------------------------------------------------
-- Join (least upper bound)
-- ------------------------------------------------------------------

/--
  The join operation computes the least upper bound of two capabilities
  in the lattice.  Mirrors `Capability::join()` in `src/types.rs`.
  Join is commutative, associative, and idempotent (the lattice is
  a meet-semilattice through `≤` and a join-semilattice through `⊔`).
-/
def join (a b : Cap) : Cap :=
  match a, b with
  | .LinearIso, .LinearIso => .LinearIso
  | .LinearIso, .Iso       => .Iso
  | .Iso,       .LinearIso => .Iso
  | .LinearIso, .Trn       => .Trn
  | .Trn,       .LinearIso => .Trn
  | .LinearIso, .Ref       => .Ref
  | .Ref,       .LinearIso => .Ref
  | .LinearIso, .Val       => .Val
  | .Val,       .LinearIso => .Val
  | .LinearIso, .Box       => .Box
  | .Box,       .LinearIso => .Box
  | .Linear,    .Linear    => .Linear
  | .Linear,    .Val       => .Val
  | .Val,       .Linear    => .Val
  | .Linear,    .LinearIso => .Val
  | .LinearIso, .Linear    => .Val
  | .Linear,    .Iso       => .Val
  | .Iso,       .Linear    => .Val
  | .Linear,    .Trn       => .Val
  | .Trn,       .Linear    => .Val
  | .Linear,    .Ref       => .Box
  | .Ref,       .Linear    => .Box
  | .Linear,    .Box       => .Box
  | .Box,       .Linear    => .Box
  | .Iso,       .Iso       => .Iso
  | .Iso,       .Trn       => .Trn
  | .Trn,       .Iso       => .Trn
  | .Trn,       .Trn       => .Trn
  | .Iso,       .Ref       => .Ref
  | .Ref,       .Iso       => .Ref
  | .Trn,       .Ref       => .Ref
  | .Ref,       .Trn       => .Ref
  | .Ref,       .Ref       => .Ref
  | .Iso,       .Val       => .Val
  | .Val,       .Iso       => .Val
  | .Trn,       .Val       => .Val
  | .Val,       .Trn       => .Val
  | .Val,       .Val       => .Val
  | .Ref,       .Val       => .Box
  | .Val,       .Ref       => .Box
  | .Iso,       .Box       => .Box
  | .Box,       .Iso       => .Box
  | .Trn,       .Box       => .Box
  | .Box,       .Trn       => .Box
  | .Ref,       .Box       => .Box
  | .Box,       .Ref       => .Box
  | .Val,       .Box       => .Box
  | .Box,       .Val       => .Box
  | .Box,       .Box       => .Box
  -- Tag retains identity but carries no read/write authority: every cap
  -- may be downgraded to Tag, never implicitly upgraded from Tag.
  | .Tag,       _          => .Tag
  | _,          .Tag       => .Tag

-- ------------------------------------------------------------------
-- Subtyping (partial order)
-- ------------------------------------------------------------------

/--
  `a ≤ b` iff the join of a and b is exactly b.
  Mirrors `Capability::is_subtype_of()`.
-/
def le (a b : Cap) : Bool := join a b == b

-- ------------------------------------------------------------------
-- Sendability for actor boundaries
-- ------------------------------------------------------------------

/--
  `a` is sendable iff values with capability `a` can be safely sent
  to another actor (the value is immutable and alias-tracked).
  Mirrors `Capability::is_sendable()` → `LinearIso | Linear | Iso | Val | Tag`.
-/
def is_sendable (a : Cap) : Bool :=
  match a with
  | .LinearIso | .Linear | .Iso | .Val | .Tag => true
  | _ => false

/--
  `a` is isolated iff values with capability `a` can be sent AND
  provide full state isolation (unique ownership).
  Mirrors `Capability::is_isolated()` → `LinearIso | Linear | Iso | Val | Tag`.
-/
def is_isolated (a : Cap) : Bool :=
  match a with
  | .LinearIso | .Linear | .Iso | .Val | .Tag => true
  | _ => false

-- ------------------------------------------------------------------
-- Linear-to-iso promotion
-- ------------------------------------------------------------------

/--
  Discharge linear tracking: LinearIso → Iso, Linear → Val.
  Used when a linear value is consumed and the obligation is satisfied.
  Mirrors `Capability::discharge_linear()`.
-/
def discharge_linear (a : Cap) : Cap :=
  match a with
  | .LinearIso => .Iso
  | .Linear    => .Val
  | c          => c

-- ------------------------------------------------------------------
-- Lattice theorems (open proofs)
-- ------------------------------------------------------------------

/--
  **Theorem 1:** `join` is associative:
  `join (join a b) c == join a (join b c)` for all `a`, `b`, `c`.
-/
theorem join_assoc : ∀ (a b c : Cap), join (join a b) c = join a (join b c) := by
  intro a b c
  cases a <;> cases b <;> cases c <;> rfl

/--
  **Theorem 2:** `join` is commutative:
  `join a b == join b a` for all `a`, `b`.
-/
theorem join_comm : ∀ (a b : Cap), join a b = join b a := by
  intro a b
  cases a <;> cases b <;> rfl

/--
  **Theorem 3:** `join` is idempotent:
  `join a a = a` for all `a`.
-/
theorem join_idem : ∀ a : Cap, join a a = a := by
  intro a
  cases a <;> rfl

/--
  **Theorem: Sendable Capabilities are Safe for Actor Boundaries**

  If the runtime permits a value `v : τ @ cap` to cross an actor
  boundary (`is_sendable cap = true`), then either:
  1. `cap ≤ Val` (immutable or isolated-transfer compatible), or
  2. `cap = Tag` (tagged pointer — safe to copy, no dereference).

  `Iso` may cross by ownership transfer; `Trn` and `Ref` must NOT
  cross an actor boundary.

  **Divergence note (2026-07):** The original statement
  `∀ cap, is_sendable cap → le cap .Val` is **false** for `cap = Tag`:
  `is_sendable Tag = true` but `le Tag Val = false`.  `Tag` is
  sendable because tagged pointers carry no ownership and can be
  safely copied across actor boundaries without dereferencing, but
  `Tag` is not a subtype of `Val` in the lattice (it sits at the
  top of the subtype order, as the least-privileged view). The corrected statement uses a
  disjunction to capture both cases.
-/
theorem cap_sendable : ∀ (cap : Cap), is_sendable cap = true → (le cap .Val = true ∨ cap = .Tag) := by
  intro cap h
  have h' := h
  cases cap <;> simp [is_sendable, le, join] at h' ⊢
  <;> first | rfl | trivial | done

theorem discharge_sendable : ∀ (cap : Cap), is_sendable cap → is_sendable (discharge_linear cap) := by
  intro cap h
  cases cap <;> simp [is_sendable, discharge_linear] at h ⊢

end Cap

-- ==================================================================
-- CAPABILITY-ANNOTATED TYPING JUDGMENT  Γ ⊢ e : τ @ cap
-- ==================================================================

/--
  Capability-aware context: each binding carries a type and a
  capability.  Extends the base `Context` from `types.lean` with
  capability annotations.  In a full implementation, `Scheme` would
  also carry capability parameters; here we keep the capability
  explicit in the binding for clarity.
-/
abbrev CapContext := List (Name × Ty × Cap)

/-- Look up a variable in the capability context. -/
def CapContext.lookup (Γ : CapContext) (x : Name) : Option (Ty × Cap) :=
  match Γ with
  | [] => none
  | (y, τ, c) :: rest => if x == y then some (τ, c) else rest.lookup x

/-- The empty capability context. -/
def CapContext.empty : CapContext := []

-- ------------------------------------------------------------------
-- Typing rules
-- ------------------------------------------------------------------

/--
  `HasTypeCap Γ e τ cap` — in context `Γ`, expression `e` has type `τ`
  with capability `cap`.

  Rules:
  - `tVar`:       variable lookup, capability from binding
  - `tLit{Int,Bool,String}`: literals are always `Val` (sendable, immutable)
  - `tLambda`:    closures are `Val` (sendable, immutable reference)
  - `tApp`:       application joins function and argument capabilities
  - `tLet`:       let-binding propagates the body's capability
  - `tIf`:        conditional joins branch capabilities
  - `tSend`:      send requires sendable capability (hypothetical — needs Expr.send)
  - `tSpawn`:     spawned actor ref is `Tag` (hypothetical — needs Expr.spawn)

  The judgment mirrors `HasType` from `types.lean` but adds capability
  propagation through join at merge points and capability checks at
  actor boundaries.
-/
inductive HasTypeCap : CapContext → Expr → Ty → Cap → Prop where

-- ** Variable **
| tVar : ∀ {Γ x τ cap},
    Γ.lookup x = some (τ, cap) →
    HasTypeCap Γ (.var x) τ cap

-- ** Literals **
| tLitInt : ∀ {Γ n},
    HasTypeCap Γ (.litInt n) .int .Val
| tLitBool : ∀ {Γ b},
    HasTypeCap Γ (.litBool b) .bool .Val
| tLitString : ∀ {Γ s},
    HasTypeCap Γ (.litString s) .string .Val

-- ** Lambda (closures are Val — safe to send) **
| tLambda : ∀ {Γ x τ₁ e τ₂ cap₁ cap₂},
    HasTypeCap ((x, τ₁, cap₁) :: Γ) e τ₂ cap₂ →
    HasTypeCap Γ (.lambda x τ₁ e) (.fn τ₁ τ₂) .Val

-- ** Application (join capabilities of function and argument) **
| tApp : ∀ {Γ e₁ e₂ τ₁ τ₂ cap₁ cap₂},
    HasTypeCap Γ e₁ (.fn τ₂ τ₁) cap₁ →
    HasTypeCap Γ e₂ τ₂ cap₂ →
    HasTypeCap Γ (.app e₁ e₂) τ₁ (Cap.join cap₁ cap₂)

-- ** Let (generalize bound type, propagate body capability) **
| tLet : ∀ {Γ x e₁ e₂ τ₁ τ₂ cap₁ cap₂},
    HasTypeCap Γ e₁ τ₁ cap₁ →
    HasTypeCap ((x, τ₁, cap₁) :: Γ) e₂ τ₂ cap₂ →
    HasTypeCap Γ (.letIn x e₁ e₂) τ₂ cap₂

-- ** If (join branch capabilities at merge point) **
| tIf : ∀ {Γ e₁ e₂ e₃ τ cap₁ cap₂ cap₃},
    HasTypeCap Γ e₁ .bool cap₁ →
    HasTypeCap Γ e₂ τ cap₂ →
    HasTypeCap Γ e₃ τ cap₃ →
    HasTypeCap Γ (.ifThenElse e₁ e₂ e₃) τ (Cap.join cap₂ cap₃)

-- ** Send: message crossing actor boundary requires sendability **
-- Note: `Expr` does not yet have a `send` constructor.  This rule is
-- stated for the capability discipline completeness and would take
-- `Expr.send e` as its subject when `Expr` is extended.
| tSend : ∀ {Γ e τ cap},
    HasTypeCap Γ e τ cap →
    Cap.is_sendable cap = true →
    HasTypeCap Γ e τ cap

-- ** Spawn: spawned actor reference is always Tag (sendable) **
-- Note: `Expr` does not yet have a `spawn` constructor.  When added,
-- this rule would type `spawn { e }` at some actor type with `Tag`.
| tSpawn : ∀ {Γ e τ cap},
    HasTypeCap Γ e τ cap →
    HasTypeCap Γ e τ .Tag

-- ==================================================================
-- LINEAR-ISO CONSUMPTION TRACKING
-- ==================================================================

/--
  `consumed Γ x` holds iff `x` is not present in the capability
  context `Γ`.  In the full linear typing discipline (which refines
  `HasTypeCap` to track input *and output* contexts), a linear
  binding (`LinearIso` or `Linear`) is removed from the output
  context after its single use.  `consumed` checks that removal.

  At merge points (if/else branches), both paths must produce the
  same output context — i.e., both consume the same linear bindings.
-/
def consumed (Γ : CapContext) (x : Name) : Bool :=
  Γ.lookup x == none

/--
  **Theorem: Linear bindings are consumed at most once.**

  **Honesty note (2026-08-14):** the statement below is *false* under the
  single-context `HasTypeCap` judgment.  Counterexample: take
  `Γ = [(x, τ₀, .Val)]` (already binds `x`) and `e = .litInt 0`; then
  `HasTypeCap ((x, τ, .LinearIso) :: Γ) e .int .Val` holds by `tLitInt`
  for *any* context, but `consumed Γ x = (Γ.lookup x == none) = false`.
  The property is therefore not a theorem of the single-context judgment:
  that judgment is a static environment with no *output* context, so it
  cannot express "`x` was consumed and does not persist for further use".

  The correct statement requires context-splitting semantics — a judgment
  `Γ ⊢ e : τ / Γ'` carrying input *and* output contexts, where a
  `LinearIso` binding is removed from `Γ'` after its single use.  This is
  the RFC 0003 Item 2 contingency: the split-context refinement is stated
  as a conjecture with a documented proof plan rather than silently
  asserted against a judgment that cannot express it.
-/
theorem linear_at_most_once : ∀ (Γ : CapContext) (x : Name) (τ : Ty) (e : Expr) (τ' : Ty) (cap : Cap),
    HasTypeCap ((x, τ, .LinearIso) :: Γ) e τ' cap →
    consumed Γ x = true := by
  intro Γ x τ e τ' cap h
  sorry

end Nulang
