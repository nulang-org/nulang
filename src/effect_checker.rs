//! Effect checker and capability analyzer for Nulang.
//!
//! This module implements:
//! - Effect inference: given an expression, infer its effect row (what effects it may perform).
//! - Effect checking: verify that an expression's effects are subsumed by an allowed effect row.
//! - Capability analysis: infer the reference capability of an expression's result.
//! - Capability checking: verify capability subtyping and sendability constraints.

use crate::ast::*;
use crate::types::*;

// Fast hashing for compiler-internal maps (keys are not attacker-controlled).
type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;
type FxHashSet<T> =
    std::collections::HashSet<T, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

// ---------------------------------------------------------------------------
// Effect Row Operations
// ---------------------------------------------------------------------------

/// Check whether every effect in `sub` is present in `sup`.
///
/// For closed rows this is simple set inclusion.  For open rows we are
/// conservative: an open row on the *sup* side may contain additional effects
/// via its row variable, while an open row on the *sub* side is assumed to
/// possibly contain any effect not explicitly listed.
pub fn effect_row_subset(sub: &EffectRow, sup: &EffectRow) -> bool {
    match (sub, sup) {
        // Closed sub, closed sup: straightforward subset check.
        (EffectRow::Closed(sub_effs), EffectRow::Closed(sup_effs)) => {
            sub_effs.iter().all(|e| sup_effs.contains(e))
        }
        // Closed sub, open sup: every concrete effect in sub must be in sup's
        // concrete list (the row variable on the sup side may cover more).
        (EffectRow::Closed(sub_effs), EffectRow::Open(sup_effs, _)) => {
            sub_effs.iter().all(|e| sup_effs.contains(e))
        }
        // Open sub, closed sup: the open row *might* contain effects beyond
        // its concrete list, so it is only a subset if the concrete list
        // itself is already a subset and the open row is empty except for the
        // variable that could introduce new effects.
        (EffectRow::Open(sub_effs, _), EffectRow::Closed(sup_effs)) => {
            sub_effs.iter().all(|e| sup_effs.contains(e))
        }
        // Open sub, open sup: both row variables could introduce arbitrary
        // effects.  We only require that the concrete effects of sub are
        // contained in the concrete effects of sup.
        (EffectRow::Open(sub_effs, _), EffectRow::Open(sup_effs, _)) => {
            sub_effs.iter().all(|e| sup_effs.contains(e))
        }
    }
}

/// Union of two effect rows (non-destructive).
pub fn effect_row_union(a: &EffectRow, b: &EffectRow) -> EffectRow {
    a.clone().combine(b.clone())
}

/// Remove a single handled effect from a row (non-destructive).
pub fn effect_row_diff(row: &EffectRow, handled: &Effect) -> EffectRow {
    row.clone().remove(handled)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a user-written effect name (from `perform Effect.op`) into the
/// built-in [`Effect`] enum when possible, otherwise create a user-defined
/// effect.
pub fn parse_effect_name(name: &str) -> Effect {
    match name {
        "IO" => Effect::IO,
        "Net" | "Http" => Effect::Net,
        "FS" => Effect::FS,
        "Array" => Effect::Array,
        "String" => Effect::String,
        "Test" => Effect::Test,
        "Rand" | "Random" => Effect::Rand,
        "Time" => Effect::Time,
        "Spawn" => Effect::Spawn,
        "Send" => Effect::Send,
        "Receive" => Effect::Receive,
        "Migrate" => Effect::Migrate,
        "STM" => Effect::STM,
        "Async" => Effect::Async,
        "Inference" => Effect::Inference,
        "Cost" => Effect::Cost,
        "Event" => Effect::Event,
        "FFI" => Effect::FFI,
        "DB" => Effect::DB,
        "Python" => Effect::Python,
        "Process" => Effect::Process,
        "System" => Effect::System,
        "Render" => Effect::Render,
        "Request" => Effect::Request,
        "Respond" => Effect::Respond,
        "Realtime" => Effect::Realtime,
        "Client" => Effect::Client,
        "Web" => Effect::Web,
        other => Effect::UserDefined(other.to_string()),
    }
}

/// Map an effect to its resource-capability category, or `None` for core
/// language effects that are never gated by `--with=`. The three coarse
/// categories mirror Aether's `fs` / `net` / `os` grants.
pub fn effect_resource_category(eff: &Effect) -> Option<&'static str> {
    match eff {
        Effect::FS => Some("fs"),
        Effect::Net => Some("net"),
        Effect::Env
        | Effect::Process
        | Effect::System
        | Effect::FFI
        | Effect::DB
        | Effect::Python => Some("os"),
        _ => None,
    }
}

/// Flatten nested `module {}` blocks into a single declaration list.
///
/// Mirrors `typechecker::flatten_decls`: modules are purely a namespacing
/// construct whose contents live in the same flat, unqualified namespace,
/// so effect and capability checking must recurse into them just like
/// type checking does.
pub fn flatten_decls(decls: &[Decl]) -> Vec<&Decl> {
    let mut out = Vec::with_capacity(decls.len());
    for decl in decls {
        match decl {
            Decl::Module { decls: inner, .. } => out.extend(flatten_decls(inner)),
            _ => out.push(decl),
        }
    }
    out
}

/// Collect the free (unbound) variable names in an expression.
/// `bound` accumulates the set of locally-bound names (parameters, let
/// bindings, etc.) and should not be included in the result.
fn free_vars(expr: &Expr, bound: &mut Vec<String>, acc: &mut Vec<String>) {
    match expr {
        Expr::Literal(_, _) => {}
        Expr::FString(parts, _) => {
            for part in parts {
                free_vars(part, bound, acc);
            }
        }
        Expr::Var(name, _) => {
            if !bound.contains(name) && !acc.contains(name) {
                acc.push(name.clone());
            }
        }
        Expr::Lambda { params, body, .. } => {
            let mut new_bound = bound.clone();
            for p in params {
                if !new_bound.contains(&p.name) {
                    new_bound.push(p.name.clone());
                }
            }
            free_vars(body, &mut new_bound, acc);
        }
        Expr::App { func, args, .. } => {
            free_vars(func, bound, acc);
            for arg in args {
                free_vars(arg, bound, acc);
            }
        }
        Expr::Let {
            name, value, body, ..
        } => {
            free_vars(value, bound, acc);
            let mut new_bound = bound.clone();
            if !new_bound.contains(name) {
                new_bound.push(name.clone());
            }
            free_vars(body, &mut new_bound, acc);
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            let mut new_bound = bound.clone();
            if !new_bound.contains(name) {
                new_bound.push(name.clone());
            }
            for p in params {
                if !new_bound.contains(&p.name) {
                    new_bound.push(p.name.clone());
                }
            }
            free_vars(value, &mut new_bound, acc);
            free_vars(body, &mut new_bound, acc);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            free_vars(cond, bound, acc);
            free_vars(then_branch, bound, acc);
            if let Some(else_b) = else_branch {
                free_vars(else_b, bound, acc);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            free_vars(scrutinee, bound, acc);
            for (pat, guard, arm_expr) in arms {
                let mut arm_bound = bound.clone();
                pat_bound_vars(pat, &mut arm_bound);
                if let Some(guard_expr) = guard {
                    free_vars(guard_expr, &mut arm_bound, acc);
                }
                free_vars(arm_expr, &mut arm_bound, acc);
            }
        }
        Expr::Block { exprs, .. } => {
            let mut block_bound = bound.clone();
            for e in exprs {
                free_vars(e, &mut block_bound, acc);
            }
        }
        Expr::Par { exprs, .. } => {
            let mut block_bound = bound.clone();
            for e in exprs {
                free_vars(e, &mut block_bound, acc);
            }
        }
        Expr::Tuple(elts, _) => {
            for e in elts {
                free_vars(e, bound, acc);
            }
        }
        Expr::Record(fields, _) => {
            for (_, e) in fields {
                free_vars(e, bound, acc);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            free_vars(base, bound, acc);
            for (_, e) in fields {
                free_vars(e, bound, acc);
            }
        }
        Expr::FieldAccess { expr: e, .. } => {
            free_vars(e, bound, acc);
        }
        Expr::Array(elts, _) => {
            for e in elts {
                free_vars(e, bound, acc);
            }
        }
        Expr::Index { arr, idx, .. } => {
            free_vars(arr, bound, acc);
            free_vars(idx, bound, acc);
        }
        Expr::Binary { left, right, .. } => {
            free_vars(left, bound, acc);
            free_vars(right, bound, acc);
        }
        Expr::Unary { expr: e, .. } => {
            free_vars(e, bound, acc);
        }
        Expr::Assign { target, value, .. } => {
            free_vars(target, bound, acc);
            free_vars(value, bound, acc);
        }
        Expr::Spawn {
            actor_type,
            init,
            target_node,
            ..
        } => {
            free_vars(actor_type, bound, acc);
            for (_, e) in init {
                free_vars(e, bound, acc);
            }
            if let Some(node) = target_node {
                free_vars(node, bound, acc);
            }
        }
        Expr::Send { actor, args, .. } => {
            free_vars(actor, bound, acc);
            for arg in args {
                free_vars(arg, bound, acc);
            }
        }
        Expr::Ask { actor, args, .. } => {
            free_vars(actor, bound, acc);
            for arg in args {
                free_vars(arg, bound, acc);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, patterns, guard, body_expr) in arms {
                let mut arm_bound = bound.clone();
                for pat in patterns {
                    pat_bound_vars(pat, &mut arm_bound);
                }
                if let Some(g) = guard {
                    free_vars(g, &mut arm_bound, acc);
                }
                free_vars(body_expr, &mut arm_bound, acc);
            }
            if let Some((ms, timeout_body)) = after {
                free_vars(ms, bound, acc);
                free_vars(timeout_body, bound, acc);
            }
        }
        Expr::SelfRef(_) => {}
        Expr::GrainRef { key, .. } => {
            free_vars(key, bound, acc);
        }
        Expr::Perform { args, .. } => {
            for arg in args {
                free_vars(arg, bound, acc);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                free_vars(arg, bound, acc);
            }
        }
        Expr::Handle { body, handlers, .. } => {
            free_vars(body, bound, acc);
            for h in handlers {
                let mut h_bound = bound.clone();
                for p in &h.params {
                    if !h_bound.contains(p) {
                        h_bound.push(p.clone());
                    }
                }
                free_vars(&h.body, &mut h_bound, acc);
            }
        }
        Expr::Migrate { actor, node, .. } => {
            free_vars(actor, bound, acc);
            free_vars(node, bound, acc);
        }
        Expr::CapAnnotate { expr: e, .. } => {
            free_vars(e, bound, acc);
        }
        Expr::TypeAnnotate { expr: e, .. } => {
            free_vars(e, bound, acc);
        }
        Expr::Pipe { left, right, .. } => {
            free_vars(left, bound, acc);
            free_vars(right, bound, acc);
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            free_vars(iterable, bound, acc);
            let mut body_bound = bound.clone();
            if !body_bound.contains(var) {
                body_bound.push(var.clone());
            }
            free_vars(body, &mut body_bound, acc);
        }
        Expr::While { cond, body, .. } => {
            free_vars(cond, bound, acc);
            free_vars(body, bound, acc);
        }
        Expr::Return(Some(e), _) => {
            free_vars(e, bound, acc);
        }
        Expr::Return(None, _) => {}
        Expr::Break(..) => {}
        Expr::Consume { expr: e, .. } => {
            free_vars(e, bound, acc);
        }
        Expr::Recover { body: b, .. } => {
            free_vars(b, bound, acc);
        }
        Expr::Defer { expr, .. } => {
            free_vars(expr, bound, acc);
        }
        Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
            free_vars(body, bound, acc);
        }
        Expr::Panic(..) => {}
        Expr::Resume { value, .. } => {
            free_vars(value, bound, acc);
        }
    }
}

/// Add all variables bound by a pattern to the `bound` accumulator.
fn pat_bound_vars(pat: &Pattern, bound: &mut Vec<String>) {
    match pat {
        Pattern::Wild => {}
        Pattern::Var(name) | Pattern::Alias(name, _) => {
            if !bound.contains(name) {
                bound.push(name.clone());
            }
        }
        Pattern::Lit(_) => {}
        Pattern::Tuple(pats) => {
            for p in pats {
                pat_bound_vars(p, bound);
            }
        }
        Pattern::Record(fields) => {
            for (_, p) in fields {
                pat_bound_vars(p, bound);
            }
        }
        Pattern::Variant(_, Some(inner)) => {
            pat_bound_vars(inner, bound);
        }
        Pattern::Variant(_, None) => {}
    }
}

// ---------------------------------------------------------------------------
// Effect Context
// ---------------------------------------------------------------------------

/// Context used during effect inference.
///
/// Tracks the set of effects that are currently allowed (e.g. from a function
/// signature) as well as which handlers are installed (so that `perform`
/// operations for those effects need not appear in the final row).
#[derive(Debug, Clone)]
pub struct EffectContext {
    /// Effects that the surrounding code permits.
    pub allowed_effects: EffectRow,
    /// Effects that are currently handled by an enclosing `handle` expression.
    pub handlers: Vec<Effect>,
}

impl EffectContext {
    /// Create a new context with no allowed effects and no handlers.
    pub fn empty() -> Self {
        EffectContext {
            allowed_effects: EffectRow::empty(),
            handlers: Vec::new(),
        }
    }

    /// Create a context that allows the given effect row.
    pub fn with_allowed(allowed: EffectRow) -> Self {
        EffectContext {
            allowed_effects: allowed,
            handlers: Vec::new(),
        }
    }

    /// Extend with an additional handler (used when descending into a
    /// `handle` block).
    pub fn with_handler(&self, eff: Effect) -> Self {
        let mut ctx = self.clone();
        ctx.handlers.push(eff);
        ctx
    }
}

// ---------------------------------------------------------------------------
// Effect Checker
// ---------------------------------------------------------------------------

/// Stateful effect checker.
///
/// Accumulates error messages so that multiple violations can be reported.
pub struct EffectChecker {
    /// Accumulated diagnostics (errors + warnings).
    pub diagnostics: Vec<String>,
    /// Effect rows of module-level functions, keyed by name. Populated by
    /// [`EffectChecker::register_function_rows`] before bodies are checked,
    /// so that a direct call site (`Expr::App` on a `Var`) propagates the
    /// callee's declared or inferred row (SPEC2 §4.9).
    fn_rows: FxHashMap<String, EffectRow>,
    /// Names currently bound by local constructs (let bindings, lambda
    /// parameters, pattern variables, ...). A locally-bound name shadows a
    /// same-named module function, so calls through it are not charged the
    /// module function's effect row.
    shadowed: Vec<String>,
    /// Granted resource-capability categories (`fs`, `net`, `os`). `None`
    /// means no gate is active (standalone programs run with full access).
    /// `Some(grants)` means a resource effect is only legal when its category
    /// is granted; core language effects (IO, Spawn, Send, ...) are never
    /// gated. Populated from the `--with=` CLI flag.
    resource_grants: Option<FxHashSet<String>>,
}

impl EffectChecker {
    /// Look up the inferred effect row of a module-level function.
    pub fn function_row(&self, name: &str) -> Option<&EffectRow> {
        self.fn_rows.get(name)
    }

    pub fn new() -> Self {
        EffectChecker {
            diagnostics: Vec::new(),
            fn_rows: FxHashMap::default(),
            shadowed: Vec::new(),
            resource_grants: None,
        }
    }

    /// Enable the resource-capability gate with the given granted categories
    /// (`fs`, `net`, `os`). Called by the CLI when `--with=` is supplied.
    pub fn set_resource_grants(&mut self, grants: &[String]) {
        self.resource_grants = Some(grants.iter().cloned().collect());
    }

    /// Infer `expr` while treating `names` as locally bound, so direct calls
    /// to same-named module functions are not charged the module function's
    /// effect row while the binding is in scope.
    fn infer_with_bound(
        &mut self,
        ctx: &EffectContext,
        names: &[String],
        expr: &Expr,
    ) -> NuResult<EffectRow> {
        let base = self.shadowed.len();
        self.shadowed.extend(names.iter().cloned());
        let result = self.infer_effects(ctx, expr);
        self.shadowed.truncate(base);
        result
    }

    /// Infer the effect row of an expression.
    ///
    /// Returns the (upper-bound) effect row describing what effects the
    /// expression may perform.
    pub fn infer_effects(&mut self, ctx: &EffectContext, expr: &Expr) -> NuResult<EffectRow> {
        match expr {
            // Literals and variables are pure.
            Expr::Literal(_, _) => Ok(EffectRow::empty()),
            Expr::FString(parts, _) => {
                let mut merged = EffectRow::empty();
                for part in parts {
                    merged = merged.combine(self.infer_effects(ctx, part)?);
                }
                Ok(merged)
            }
            Expr::Var(_, _) => Ok(EffectRow::empty()),

            // Lambda: effects are given by its annotation, or inferred from the
            // body if unannotated. If annotated, the body must not perform effects
            // beyond the annotation. Parameters shadow same-named module
            // functions inside the body.
            Expr::Lambda {
                params,
                body,
                effect,
                ..
            } => {
                let base = self.shadowed.len();
                self.shadowed.extend(params.iter().map(|p| p.name.clone()));
                let result = if let Some(ann) = effect {
                    let lambda_ctx = EffectContext::with_allowed(ann.clone());
                    self.check_effects(&lambda_ctx, body, ann)
                        .map(|_| ann.clone())
                } else {
                    self.infer_effects(ctx, body)
                };
                self.shadowed.truncate(base);
                result
            }

            // Application: effects of function + arguments, plus the callee's
            // own row when this is a direct call to a known module-level
            // function (SPEC2 §4.9). Locally-bound names shadow module
            // functions and are not charged.
            Expr::App { func, args, .. } => {
                let mut row = self.infer_effects(ctx, func)?;
                for arg in args {
                    row = effect_row_union(&row, &self.infer_effects(ctx, arg)?);
                }
                if let Expr::Var(name, _) = func.as_ref() {
                    if !self.shadowed.contains(name) {
                        if let Some(callee_row) = self.fn_rows.get(name) {
                            row = effect_row_union(&row, callee_row);
                        }
                    }
                }
                Ok(row)
            }

            // Let: effects of value + effects of body. The binding shadows a
            // same-named module function inside the body.
            Expr::Let {
                name, value, body, ..
            } => {
                let val_row = self.infer_effects(ctx, value)?;
                let body_row = self.infer_with_bound(ctx, std::slice::from_ref(name), body)?;
                Ok(effect_row_union(&val_row, &body_row))
            }

            // Let-rec: similar to let, but the binding is recursive (in scope
            // in the value as well as the body).
            Expr::LetRec {
                name, value, body, ..
            } => {
                let names = std::slice::from_ref(name);
                let val_row = self.infer_with_bound(ctx, names, value)?;
                let body_row = self.infer_with_bound(ctx, names, body)?;
                Ok(effect_row_union(&val_row, &body_row))
            }

            // If: union of condition, then-branch, and else-branch effects.
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let mut row = self.infer_effects(ctx, cond)?;
                row = effect_row_union(&row, &self.infer_effects(ctx, then_branch)?);
                if let Some(else_b) = else_branch {
                    row = effect_row_union(&row, &self.infer_effects(ctx, else_b)?);
                }
                Ok(row)
            }

            // Match: union of scrutinee and all arm effects. Pattern-bound
            // variables shadow same-named module functions inside their arm.
            Expr::Match {
                scrutinee, arms, ..
            } => {
                let mut row = self.infer_effects(ctx, scrutinee)?;
                for (pat, guard, arm_expr) in arms {
                    let mut names = Vec::new();
                    pat_bound_vars(pat, &mut names);
                    if let Some(guard_expr) = guard {
                        let guard_row = self.infer_with_bound(ctx, &names, guard_expr)?;
                        row = effect_row_union(&row, &guard_row);
                    }
                    let arm_row = self.infer_with_bound(ctx, &names, arm_expr)?;
                    row = effect_row_union(&row, &arm_row);
                }
                Ok(row)
            }

            // Block: union of all sub-expression effects.
            Expr::Block { exprs, .. } => {
                let mut row = EffectRow::empty();
                for e in exprs {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Par: independence annotation, sequential effect union.
            Expr::Par { exprs, .. } => {
                let mut row = EffectRow::empty();
                for e in exprs {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Tuple: union of element effects.
            Expr::Tuple(elts, _) => {
                let mut row = EffectRow::empty();
                for e in elts {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Record: union of field effects.
            Expr::Record(fields, _) => {
                let mut row = EffectRow::empty();
                for (_, e) in fields {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Record update: effects of base plus override fields.
            Expr::RecordUpdate { base, fields, .. } => {
                let mut row = self.infer_effects(ctx, base)?;
                for (_, e) in fields {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Field access: effects of the base expression only.
            Expr::FieldAccess { expr: e, .. } => self.infer_effects(ctx, e),

            // Array: union of element effects.
            Expr::Array(elts, _) => {
                let mut row = EffectRow::empty();
                for e in elts {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                Ok(row)
            }

            // Array index: effects of array + index expressions.
            Expr::Index { arr, idx, .. } => {
                let r1 = self.infer_effects(ctx, arr)?;
                let r2 = self.infer_effects(ctx, idx)?;
                Ok(effect_row_union(&r1, &r2))
            }

            // Binary: union of left and right. Range op additionally
            // introduces the Array effect (range desugars to array allocation).
            Expr::Binary {
                op, left, right, ..
            } => {
                let r1 = self.infer_effects(ctx, left)?;
                let r2 = self.infer_effects(ctx, right)?;
                let mut r = effect_row_union(&r1, &r2);
                if *op == BinOp::Range {
                    r = effect_row_union(&r, &EffectRow::singleton(Effect::Array));
                }
                Ok(r)
            }

            // Unary: effects of the operand.
            Expr::Unary { expr: e, .. } => self.infer_effects(ctx, e),

            // Assignment: effects of target + value.
            Expr::Assign { target, value, .. } => {
                let r1 = self.infer_effects(ctx, target)?;
                let r2 = self.infer_effects(ctx, value)?;
                Ok(effect_row_union(&r1, &r2))
            }

            Expr::Spawn {
                actor_type,
                init,
                target_node,
                ..
            } => {
                let mut row = EffectRow::singleton(Effect::Spawn);
                row = effect_row_union(&row, &self.infer_effects(ctx, actor_type)?);
                for (_, e) in init {
                    row = effect_row_union(&row, &self.infer_effects(ctx, e)?);
                }
                if let Some(node) = target_node {
                    row = effect_row_union(&row, &self.infer_effects(ctx, node)?);
                }
                Ok(row)
            }

            // Send: adds the Send effect + effects of actor and arguments.
            Expr::Send {
                actor, args, span, ..
            } => {
                let mut row = EffectRow::singleton(Effect::Send);
                row = effect_row_union(&row, &self.infer_effects(ctx, actor)?);
                for arg in args {
                    row = effect_row_union(&row, &self.infer_effects(ctx, arg)?);
                }
                // Also check that the Send capability requirement is met by
                // the actor expression (it must be sendable in some form).
                // We don't have a full type env here, so we defer to the
                // capability analyser for that.
                let _ = span;
                Ok(row)
            }

            // Ask: adds Send + Receive effects + actor and argument effects.
            Expr::Ask {
                actor, args, span, ..
            } => {
                let send_row = EffectRow::singleton(Effect::Send);
                let recv_row = EffectRow::singleton(Effect::Receive);
                let mut row = effect_row_union(&send_row, &recv_row);
                row = effect_row_union(&row, &self.infer_effects(ctx, actor)?);
                for arg in args {
                    row = effect_row_union(&row, &self.infer_effects(ctx, arg)?);
                }
                let _ = span;
                Ok(row)
            }

            // Receive: adds the Receive effect. Arm parameters shadow
            // same-named module functions inside their arm body. The optional
            // `after` clause contributes the effects of its timeout
            Expr::Receive { arms, after, .. } => {
                let mut row = EffectRow::singleton(Effect::Receive);
                for (_, patterns, guard, body_expr) in arms {
                    let mut names: Vec<String> = Vec::new();
                    for pat in patterns {
                        pat_bound_vars(pat, &mut names);
                    }
                    let arm_row = self.infer_with_bound(ctx, &names, body_expr)?;
                    row = effect_row_union(&row, &arm_row);
                    if let Some(g) = guard {
                        let guard_row = self.infer_with_bound(ctx, &names, g)?;
                        row = effect_row_union(&row, &guard_row);
                    }
                }
                if let Some((ms, timeout_body)) = after {
                    row = effect_row_union(&row, &self.infer_effects(ctx, ms)?);
                    row = effect_row_union(&row, &self.infer_effects(ctx, timeout_body)?);
                }
                Ok(row)
            }

            // Self reference: pure (just a variable-like read).
            Expr::SelfRef(_) => Ok(EffectRow::empty()),

            // Virtual actor reference: pure at the effect-row level; the
            // underlying runtime dispatch is a built-in, not a user effect.
            Expr::GrainRef { key, .. } => self.infer_effects(ctx, key),

            // Perform effect: adds the named effect to the row.
            Expr::Perform {
                effect,
                op,
                args,
                span,
            } => {
                let eff = parse_effect_name(effect);

                // Check whether this effect is handled by an enclosing handler.
                let is_handled = ctx.handlers.iter().any(|h| {
                    h == &eff || matches!((h, &eff), (Effect::UserDefined(a), Effect::UserDefined(b)) if a == b)
                });

                // Validate that the operation name is sensible (basic check).
                if op.is_empty() {
                    return Err(NuError::effect_error(
                        format!("perform of effect '{}' has empty operation name", effect),
                        *span,
                    ));
                }

                let mut row = if is_handled {
                    EffectRow::empty()
                } else {
                    EffectRow::singleton(eff)
                };

                // Add argument effects.
                for arg in args {
                    row = effect_row_union(&row, &self.infer_effects(ctx, arg)?);
                }

                Ok(row)
            }

            // Emit event: contributes an Event effect plus argument effects.
            Expr::Emit { args, .. } => {
                let mut row = EffectRow::singleton(Effect::Event);
                for arg in args {
                    row = effect_row_union(&row, &self.infer_effects(ctx, arg)?);
                }
                Ok(row)
            }

            // Handle: body effects minus handled effects, plus handler body effects.
            Expr::Handle {
                body,
                handlers,
                span,
            } => {
                // Compute which effects are handled.
                let mut handled_effs: Vec<Effect> = Vec::new();
                for h in handlers {
                    handled_effs.push(parse_effect_name(&h.effect_name));
                }

                // Build a context where the handled effects are registered.
                let mut inner_ctx = ctx.clone();
                for eff in &handled_effs {
                    inner_ctx.handlers.push(eff.clone());
                }

                // Infer body effects under the extended handler context.
                let mut row = self.infer_effects(&inner_ctx, body)?;

                // Remove handled effects from the resulting row.
                for eff in &handled_effs {
                    row = effect_row_diff(&row, eff);
                }

                // Add effects of each handler body. Handler parameters shadow
                // same-named module functions inside the handler body.
                for h in handlers {
                    let h_row = self.infer_with_bound(ctx, &h.params, &h.body)?;
                    row = effect_row_union(&row, &h_row);
                }

                let _ = span;
                Ok(row)
            }

            // Migrate: adds Migrate effect + actor and node effects.
            Expr::Migrate { actor, node, .. } => {
                let mut row = EffectRow::singleton(Effect::Migrate);
                row = effect_row_union(&row, &self.infer_effects(ctx, actor)?);
                row = effect_row_union(&row, &self.infer_effects(ctx, node)?);
                Ok(row)
            }

            // Capability annotation: just the inner expression's effects.
            Expr::CapAnnotate { expr: e, .. } => self.infer_effects(ctx, e),

            // Type annotation: just the inner expression's effects.
            Expr::TypeAnnotate { expr: e, .. } => self.infer_effects(ctx, e),

            // Pipe: effects of left + right.
            Expr::Pipe { left, right, .. } => {
                let r1 = self.infer_effects(ctx, left)?;
                let r2 = self.infer_effects(ctx, right)?;
                Ok(effect_row_union(&r1, &r2))
            }

            // For comprehension: effects of iterable + body. The loop
            // variable shadows a same-named module function inside the body.
            Expr::For {
                var,
                iterable,
                body,
                span,
            } => {
                let r1 = self.infer_effects(ctx, iterable)?;
                let r2 = self.infer_with_bound(ctx, std::slice::from_ref(var), body)?;
                let _ = span;
                Ok(effect_row_union(&r1, &r2))
            }

            // While loop: effects of cond + body. Evaluates to unit.
            Expr::While { cond, body, span } => {
                let r1 = self.infer_effects(ctx, cond)?;
                let r2 = self.infer_effects(ctx, body)?;
                let _ = span;
                Ok(effect_row_union(&r1, &r2))
            }
            // Return: effects of the returned expression (if any).
            Expr::Return(Some(e), _) => self.infer_effects(ctx, e),
            Expr::Return(None, _) => Ok(EffectRow::empty()),

            // Break: no effects (it transfers control, doesn't perform an effect).
            Expr::Break(..) => Ok(EffectRow::empty()),

            // Consume: effects of the consumed expression.
            Expr::Consume { expr: e, .. } => self.infer_effects(ctx, e),

            // Recover: effects of the recovery body.
            Expr::Recover { body: b, .. } => self.infer_effects(ctx, b),
            // Defer: effects of the deferred expression.
            Expr::Defer { expr: e, .. } => self.infer_effects(ctx, e),
            Expr::Hide { body, .. } | Expr::Seal { body, .. } => self.infer_effects(ctx, body),
            Expr::Panic(..) => Ok(EffectRow::empty()),
            Expr::Resume { .. } => Ok(EffectRow::empty()),
        }
    }

    /// Check that an expression's effects are subsumed by a given effect row.
    ///
    /// This infers the expression's effects and then verifies subset inclusion.
    /// On failure, a [`NuError::EffectError`] is returned.
    pub fn check_effects(
        &mut self,
        ctx: &EffectContext,
        expr: &Expr,
        allowed: &EffectRow,
    ) -> NuResult<()> {
        let inferred = self.infer_effects(ctx, expr)?;
        if !effect_row_subset(&inferred, allowed) {
            // Identify which effects are not allowed for a better error message.
            let offending: Vec<String> = inferred
                .effects()
                .iter()
                .filter(|e| !allowed.contains(e))
                .map(|e| format!("{}", e))
                .collect();
            let span = expr_span(expr);
            let msg = if offending.is_empty() {
                format!(
                    "effects {} are not a subset of allowed effects {}",
                    format_row(&inferred),
                    format_row(allowed)
                )
            } else {
                format!(
                    "effects {} contain disallowed effect(s): {} (allowed: {})",
                    format_row(&inferred),
                    offending.join(", "),
                    format_row(allowed)
                )
            };
            self.diagnostics.push(msg.clone());
            Err(NuError::EffectError {
                msg,
                span,
                missing_effects: if offending.is_empty() {
                    None
                } else {
                    Some(offending)
                },
                allowed_effects: Some(format_row(allowed)),
            })
        } else {
            Ok(())
        }
    }

    /// Pass 1 of module effect checking: record each function's effect row.
    ///
    /// Annotated functions contribute their declared row; unannotated
    /// functions start at the empty row and are then iterated to a fixpoint
    /// so that call chains propagate callee effects (SPEC2 §4.7/§4.9). Rows
    /// are finite sets that only grow via union, so the iteration reaches
    /// the fixpoint within `n` rounds for `n` functions — an effect `k`
    /// calls away is picked up after `k` rounds, and recursive or
    /// mutually-recursive functions simply saturate.
    ///
    /// Takes the flattened declaration list (see [`flatten_decls`]). After
    /// this call, [`EffectChecker::infer_effects`] unions the callee row at
    /// direct call sites (`Expr::App` on a `Var`).
    pub fn register_function_rows(&mut self, decls: &[&Decl]) -> NuResult<()> {
        let ctx = EffectContext::empty();
        for decl in decls {
            if let Decl::Function { name, effect, .. } = decl {
                let row = effect.clone().unwrap_or_else(EffectRow::empty);
                self.fn_rows.insert(name.clone(), row);
            }
        }
        for _ in 0..self.fn_rows.len() {
            let mut changed = false;
            for decl in decls {
                if let Decl::Function {
                    name,
                    effect: None,
                    body,
                    ..
                } = decl
                {
                    let inferred = self.infer_effects(&ctx, body)?;
                    let entry = self
                        .fn_rows
                        .entry(name.clone())
                        .or_insert_with(EffectRow::empty);
                    // Grow only when the inferred row contributes an effect
                    // not already recorded (a plain `!=` on the union would
                    // never stabilize, since union appends duplicates).
                    if !effect_row_subset(&inferred, entry) {
                        *entry = effect_row_union(entry, &inferred);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        Ok(())
    }

    /// Pass 2 of module effect checking: enforce a single (flattened)
    /// declaration's bodies.
    ///
    /// Bodies with a declared effect row (`! E`) are checked against it;
    /// un-annotated bodies are inference-only (their row is already recorded
    /// for callers by [`EffectChecker::register_function_rows`]).
    pub fn check_decl(&mut self, decl: &Decl) -> NuResult<()> {
        let ctx = EffectContext::empty();
        match decl {
            Decl::Function { body, effect, .. } => match effect {
                Some(allowed) => self.check_effects(&ctx, body, allowed),
                None => self.infer_effects(&ctx, body).map(|_| ()),
            },
            Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for b in behaviors {
                    match &b.effect {
                        Some(allowed) => self.check_effects(&ctx, &b.body, allowed)?,
                        None => self.infer_effects(&ctx, &b.body).map(|_| ())?,
                    }
                }
                for (_, _, _, default) in state_fields {
                    self.infer_effects(&ctx, default)?;
                }
                for (_, expr) in init {
                    self.infer_effects(&ctx, expr)?;
                }
                Ok(())
            }
            Decl::StateMachine {
                name,
                states,
                events,
                entry_hooks,
                exit_hooks,
                span,
            } => {
                // Effect-check the desugared form exactly like an actor.
                let actor = crate::ast::desugar_state_machine(
                    name,
                    states,
                    events,
                    entry_hooks,
                    exit_hooks,
                    *span,
                );
                self.check_decl(&actor)
            }
            Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    let steps: &[WorkflowStep] = match item {
                        WorkflowItem::Step(s) => std::slice::from_ref(s),
                        WorkflowItem::Parallel(steps) => steps,
                    };
                    for step in steps {
                        self.infer_effects(&ctx, &step.body)?;
                        if let Some(comp) = &step.compensate {
                            self.infer_effects(&ctx, comp)?;
                        }
                    }
                }
                if let Some(comp) = compensate {
                    self.infer_effects(&ctx, comp)?;
                }
                Ok(())
            }
            // Agent declarations carry only configuration, no expression bodies.
            _ => Ok(()),
        }
    }

    /// Full module effect check: flatten nested `module {}` blocks (so their
    /// declarations are checked just like top-level ones), register function
    /// rows so callee effects propagate (pass 1), then enforce declared rows
    /// on every body (pass 2).
    pub fn check_module(&mut self, decls: &[Decl]) -> NuResult<()> {
        let flat = flatten_decls(decls);
        for decl in &flat {
            self.emit_deprecation_warning(decl);
        }
        self.register_function_rows(&flat)?;
        self.emit_placement_warnings(&flat);
        for decl in &flat {
            self.check_decl(decl)?;
        }
        self.check_resource_grants()?;
        Ok(())
    }

    /// Enforce the resource-capability gate (active only when
    /// `set_resource_grants` was called): every resource effect performed by a
    /// module function must belong to a granted category. Core language
    /// effects (IO, Spawn, Send, ...) are never gated.
    fn check_resource_grants(&self) -> NuResult<()> {
        let Some(grants) = &self.resource_grants else {
            return Ok(());
        };
        for (name, row) in &self.fn_rows {
            for eff in row.effects() {
                if let Some(category) = effect_resource_category(eff) {
                    if !grants.contains(category) {
                        return Err(NuError::EffectError {
                            msg: format!(
                                "function '{name}' performs effect '{eff}' which requires resource capability '{category}' (not granted; grant it with --with={category})"
                            ),
                            span: Span::default(),
                            missing_effects: None,
                            allowed_effects: None,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Emit placement warnings for web-framework functions.
    /// If a function has no explicit @placement but performs web effects,
    /// infer a placement from its effect row and warn.
    fn emit_placement_warnings(&mut self, decls: &[&Decl]) {
        for decl in decls {
            let (name, annotations, declared_effect, body_span) = match decl {
                Decl::Function {
                    name,
                    annotations,
                    effect,
                    span,
                    ..
                } => (name, annotations, effect, span),
                _ => continue,
            };
            let has_explicit = annotations
                .iter()
                .any(|a| matches!(a, crate::ast::FunctionAnnotation::Placement(_)));
            if has_explicit {
                continue;
            }
            let row = match declared_effect {
                Some(r) => r.clone(),
                None => self
                    .fn_rows
                    .get(name)
                    .cloned()
                    .unwrap_or_else(EffectRow::empty),
            };
            let effects: Vec<_> = match &row {
                EffectRow::Closed(effs) => effs.clone(),
                EffectRow::Open(effs, _) => effs.clone(),
            };
            let has_request = effects.iter().any(|e| *e == Effect::Request);
            let only_render_or_web = effects
                .iter()
                .all(|e| *e == Effect::Render || *e == Effect::Web)
                && !effects.is_empty();
            if has_request {
                self.diagnostics.push(format!(
                    "warning: function '{}' has no @placement; inferred placement: server (because it performs Request)",
                    name
                ));
            } else if only_render_or_web {
                self.diagnostics.push(format!(
                    "warning: function '{}' has no @placement; inferred placement: static (because it only performs Render or Web)",
                    name
                ));
            }
            let _ = body_span; // reserved for future line/column diagnostics
        }
    }

    /// Emit a deprecation warning for a single declaration if it uses language
    /// surface scheduled for removal. See RFC 0004.
    fn emit_deprecation_warning(&mut self, decl: &Decl) {
        let (kind, name, span) = match decl {
            Decl::Agent { name, span, .. } => ("agent", name.as_str(), *span),
            Decl::Workflow { name, span, .. } => ("workflow", name.as_str(), *span),
            Decl::Database { name, span, .. } => ("database", name.as_str(), *span),
            _ => return,
        };
        self.diagnostics.push(format!(
            "warning: `{}` declaration '{}' is deprecated and will be removed in a future language version (RFC 0004). Use an `actor` with the relevant Cloud SDK library instead (e.g., `nlc.ai`, `nlc.workflow`, `nlc.storage`).",
            kind, name
        ));
        let _ = span; // span reserved for future line/column diagnostics
    }
}

impl Default for EffectChecker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Capability Context
// ---------------------------------------------------------------------------

/// Context used during capability analysis.
///
/// Maps variable names to their reference capabilities.  The `default_cap`
/// is used when a variable is not found in the bindings (e.g. for primitives).
#[derive(Debug, Clone)]
pub struct CapContext {
    /// Explicit (name, capability) bindings in scope.
    pub bindings: Vec<(String, Capability)>,
    /// Default capability to use for unbound names (typically `Val`).
    pub default_cap: Capability,
}

impl CapContext {
    /// Create an empty context with `Val` as the default.
    pub fn new() -> Self {
        CapContext {
            bindings: Vec::new(),
            default_cap: Capability::Val,
        }
    }

    /// Look up the capability of a variable by name.
    pub fn lookup(&self, name: &str) -> Capability {
        self.bindings
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, c)| *c)
            .unwrap_or(self.default_cap)
    }

    /// Bind a new variable with the given capability, returning an extended
    /// context (non-destructive).
    pub fn with_binding(&self, name: impl Into<String>, cap: Capability) -> Self {
        let mut ctx = self.clone();
        ctx.bindings.push((name.into(), cap));
        ctx
    }

    /// Bind multiple variables at once.
    pub fn with_bindings(&self, binds: &[(String, Capability)]) -> Self {
        let mut ctx = self.clone();
        for (n, c) in binds {
            ctx.bindings.push((n.clone(), *c));
        }
        ctx
    }

    /// Bind explicitly annotated capabilities of function-like parameters.
    pub fn with_params(&self, params: &[Param]) -> Self {
        let mut ctx = self.clone();
        for param in params {
            if let Some(cap) = param.cap {
                ctx.bindings.push((param.name.clone(), cap));
            }
        }
        ctx
    }
}

impl Default for CapContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Capability Analyzer
// ---------------------------------------------------------------------------

/// Stateful capability analyzer.
pub struct CapabilityAnalyzer {
    /// Accumulated diagnostics.
    pub diagnostics: Vec<String>,
    /// Spans of LinearIso/Linear variable references that were consumed
    /// during analysis (for LSP capability visualization).
    pub consumed_spans: Vec<Span>,
    /// Per-binding record of the FIRST consumption span. When a linear
    /// binding is used a second time, the error message includes both
    /// the first-use location (from this map) and the second-use location.
    pub first_consumed: FxHashMap<String, Span>,
}

impl CapabilityAnalyzer {
    /// Create a new capability analyzer.
    pub fn new() -> Self {
        CapabilityAnalyzer {
            diagnostics: Vec::new(),
            consumed_spans: Vec::new(),
            first_consumed: FxHashMap::default(),
        }
    }

    /// Infer the capability of an expression's result.
    ///
    /// Returns the most precise capability we can determine for the value
    /// produced by the expression.
    ///
    /// This is the public entry point: it runs the analysis with a fresh
    /// linear-consumption set, so consumption state never leaks between
    /// top-level calls (the frontend reuses one analyzer across declarations).
    pub fn infer_cap(&mut self, ctx: &CapContext, expr: &Expr) -> NuResult<Capability> {
        let mut consumed = FxHashSet::default();
        let cap = self.infer_cap_tracked(ctx, expr, &mut consumed)?;

        // Ensure all linear bindings in the initial context were consumed.
        for (name, binding_cap) in &ctx.bindings {
            if binding_cap.is_linear() && !consumed.contains(name) {
                let msg = format!(
                    "linear value `{}` is never used ({} bindings must be consumed exactly once — pass it to a function, `send` it, or `consume {}` it explicitly)",
                    name, binding_cap, name
                );
                self.diagnostics.push(msg.clone());
                return Err(NuError::cap_error_explained(
                    msg,
                    expr_span(expr),
                    format!(
                        "`{}` is a linear binding in the initial scope (e.g. a function parameter); every path through the body must consume it exactly once",
                        name
                    ),
                ));
            }
        }

        Ok(cap)
    }

    /// Mark a `LinearIso` binding as consumed, erroring if it already was.
    ///
    /// Any reference to a linear binding is a *use* that moves the value:
    /// passing it to a function, sending it to an actor, storing it in a
    /// structure, returning it, or capturing it in a closure all route
    /// through `Expr::Var` (or the closure-capture rule in the Lambda arm).
    fn consume_linear(
        &mut self,
        name: &str,
        span: Span,
        consumed: &mut FxHashSet<String>,
    ) -> NuResult<()> {
        // Record the span for LSP visualization regardless of error.
        self.consumed_spans.push(span);
        if !consumed.insert(name.to_string()) {
            let first_span = self.first_consumed.get(name);
            let mut msg = format!("linear value `{}` used after being consumed", name);
            if let Some(fs) = first_span {
                msg.push_str(&format!(
                    " (first consumed at line {}:{})",
                    fs.start, fs.end
                ));
            }
            msg.push_str("\nhelp: linear/lineariso bindings may be used at most once");
            msg.push_str(&format!("\nhelp: use `consume {}` to explicitly discharge the linear obligation on the first use, or restructure to avoid the second use", name));
            self.diagnostics.push(msg.clone());
            return Err(NuError::cap_error_explained(
                msg,
                span,
                "linear/lineariso bindings are moved on first use and may not be referenced again on the same path",
            ));
        }
        self.first_consumed.insert(name.to_string(), span);
        Ok(())
    }

    /// Mark an `Iso` binding as consumed after a move operation (send,
    /// ask, closure capture), erroring if it was already moved along this
    /// path.
    ///
    /// Unlike `LinearIso`/`Linear` (which are consumed on every variable
    /// reference via `Expr::Var`), plain `Iso` is consumed only at explicit
    /// ownership-transfer points.  The same `consumed` set is used so that
    /// branch merge, loop rejection, and shadowing work identically.
    fn consume_if_iso(
        &mut self,
        name: &str,
        span: Span,
        consumed: &mut FxHashSet<String>,
    ) -> NuResult<()> {
        self.consumed_spans.push(span);
        if !consumed.insert(name.to_string()) {
            let first_span = self.first_consumed.get(name);
            let mut msg = format!("iso value `{}` used after being moved", name);
            if let Some(fs) = first_span {
                msg.push_str(&format!(" (first moved at line {}:{})", fs.start, fs.end));
            }
            msg.push_str("\nhelp: iso bindings transfer ownership on send/ask");
            msg.push_str(&format!(
                "\nhelp: use `consume {}` to explicitly discharge the iso before the move, or restructure to avoid the second use",
                name
            ));
            self.diagnostics.push(msg.clone());
            return Err(NuError::cap_error_explained(
                msg,
                span,
                "an iso binding transfers ownership on send/ask/closure-capture and cannot be moved twice",
            ));
        }
        self.first_consumed.insert(name.to_string(), span);
        Ok(())
    }

    /// Recursive worker for [`infer_cap`] that tracks which `LinearIso`
    /// bindings have already been consumed along the current path.
    ///
    /// Linearity rules (conservative MVP — at-most-once use):
    /// - Referencing a variable whose capability is `LinearIso` consumes the
    ///   binding; a second reference on the same path is a `CapError`.
    /// - Branches merge conservatively: a binding is consumed after an
    ///   `if`/`match`/`receive` only if *every* fall-through path consumes
    ///   it, so a use in one branch never poisons a sibling branch.
    /// - Consuming an outer linear binding inside a `for` body errors, since
    ///   the loop may iterate more than once.
    /// - A binding that is never used is NOT an error: exactly-once
    ///   (must-use on all paths) analysis is a documented follow-up.
    fn infer_cap_tracked(
        &mut self,
        ctx: &CapContext,
        expr: &Expr,
        consumed: &mut FxHashSet<String>,
    ) -> NuResult<Capability> {
        match expr {
            // Literals are immutable values.
            Expr::Literal(_, _) => Ok(Capability::Val),
            Expr::FString(parts, _) => {
                let mut cap = Capability::Val;
                for part in parts {
                    let c = self.infer_cap_tracked(ctx, part, consumed)?;
                    if c == Capability::Iso || c == Capability::Trn {
                        cap = c;
                    }
                }
                Ok(cap)
            }

            Expr::Var(name, span) => {
                let cap = ctx.lookup(name);
                if cap.is_linear() {
                    self.consume_linear(name, *span, consumed)?;
                } else if cap == Capability::Iso && consumed.contains(name) {
                    let first_span = self.first_consumed.get(name);
                    let mut msg = format!("iso value `{}` used after being moved", name);
                    if let Some(fs) = first_span {
                        msg.push_str(&format!(" (first moved at line {}:{})", fs.start, fs.end));
                    }
                    msg.push_str("\nhelp: iso bindings transfer ownership on send/ask and may be used at most once thereafter");
                    self.diagnostics.push(msg.clone());
                    return Err(NuError::cap_error(msg, *span));
                }
                Ok(cap)
            }

            // Lambda: capability is the join of all captured free variables.
            // If there are no captures, it defaults to `Val` (a pure function
            // with no mutable state is immutable).
            Expr::Lambda {
                params, body, span, ..
            } => {
                let mut free = Vec::new();
                let mut bound: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                free_vars(body, &mut bound, &mut free);
                if free.is_empty() {
                    Ok(Capability::Val)
                } else {
                    let mut cap = ctx.lookup(&free[0]);
                    for name in &free[1..] {
                        cap = cap.join(ctx.lookup(name));
                    }
                    // Capturing a linear binding in a closure consumes it:
                    // the closure may escape or be invoked multiple times.
                    for name in &free {
                        if ctx.lookup(name).is_linear() {
                            self.consume_linear(name, *span, consumed)?;
                        }
                    }
                    // Capturing an iso binding in a closure also transfers
                    // ownership, same as send.
                    for name in &free {
                        if ctx.lookup(name) == Capability::Iso {
                            self.consume_if_iso(name, *span, consumed)?;
                        }
                    }
                    Ok(cap)
                }
            }

            // Application: conservative join of function capability and all
            // argument capabilities.
            Expr::App { func, args, .. } => {
                let mut cap = self.infer_cap_tracked(ctx, func, consumed)?;
                for arg in args {
                    cap = cap.join(self.infer_cap_tracked(ctx, arg, consumed)?);
                }
                Ok(cap)
            }

            // Let: capability of the body. A `LinearIso`/`Linear` binding
            // must be consumed exactly once: `consume_linear` (via any Var
            // reference) already rejects a *second* use; here we reject
            // *zero* uses once the body's scope closes, completing the
            // exactly-once discipline (see `spec/formal/capabilities.lean`'s
            // `linear_at_most_once` theorem and its doc comment).
            Expr::Let {
                name,
                value,
                body,
                span,
                ..
            } => {
                let val_cap = self.infer_cap_tracked(ctx, value, consumed)?;
                let body_ctx = ctx.with_binding(name.clone(), val_cap);
                // Shadowing: the new binding hides any outer binding of the
                // same name. Hide the outer consumption state while analyzing
                // the body, then restore it; the inner binding's own
                // consumption is scope-local and never leaks out.
                let outer_consumed = consumed.remove(name);
                let result = self.infer_cap_tracked(&body_ctx, body, consumed);
                // A bare rebind (`let a = x` or `let a = consume x`) is
                // transparent: evaluating `value` already discharged the
                // source binding's own obligation (via `consume_linear` in
                // the Var/Consume cases below), so `a` doesn't carry a
                // *second*, independent must-use obligation for the same
                // underlying value — only a genuinely fresh linear value
                // (a literal/call annotated `:cap lineariso`, a function
                // return, etc.) does.
                let is_transparent_rebind = match value.as_ref() {
                    Expr::Var(..) => true,
                    Expr::Consume { expr: inner, .. } => matches!(inner.as_ref(), Expr::Var(..)),
                    _ => false,
                };
                if result.is_ok()
                    && val_cap.is_linear()
                    && !is_transparent_rebind
                    && !consumed.contains(name)
                {
                    let msg = format!(
                        "linear value `{}` is never used ({} bindings must be consumed exactly once — pass it to a function, `send` it, or `consume {}` it explicitly)",
                        name, val_cap, name
                    );
                    self.diagnostics.push(msg.clone());
                    consumed.remove(name);
                    if outer_consumed {
                        consumed.insert(name.clone());
                    }
                    return Err(NuError::cap_error(msg, *span));
                }
                consumed.remove(name);
                if outer_consumed {
                    consumed.insert(name.clone());
                }
                result
            }

            // Let-rec: similar to let, but recursive.
            Expr::LetRec {
                name,
                params,
                value,
                body,
                ..
            } => {
                // Recursive binding: we approximate the binding capability as
                // the join of param capabilities (or Val if no params).
                let mut rec_cap = Capability::Val;
                for _ in params {
                    rec_cap = rec_cap.join(Capability::Val);
                }
                // `name` is bound in both the value and the body; apply the
                // same shadowing discipline as `let`.
                let outer_consumed = consumed.remove(name);
                let val_ctx = ctx.with_binding(name.clone(), rec_cap);
                let result = match self.infer_cap_tracked(&val_ctx, value, consumed) {
                    Ok(val_cap) => {
                        let body_ctx = ctx.with_binding(name.clone(), val_cap);
                        self.infer_cap_tracked(&body_ctx, body, consumed)
                    }
                    Err(e) => Err(e),
                };
                consumed.remove(name);
                if outer_consumed {
                    consumed.insert(name.clone());
                }
                result
            }

            // If: join of then and else capabilities.
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let _ = self.infer_cap_tracked(ctx, cond, consumed)?; // cond cap not part of result
                                                                      // Branch merge: analyze each branch from the same base set,
                                                                      // then keep only the bindings consumed on *every* fall-through
                                                                      // path (a use in one branch must not poison a sibling branch;
                                                                      // a missing else branch consumes nothing).
                let base = consumed.clone();
                let then_cap = self.infer_cap_tracked(ctx, then_branch, consumed)?;
                let then_set = std::mem::replace(consumed, base);
                let else_cap = match else_branch {
                    Some(else_b) => self.infer_cap_tracked(ctx, else_b, consumed)?,
                    None => then_cap,
                };
                let else_set = std::mem::take(consumed);
                *consumed = then_set.intersection(&else_set).cloned().collect();
                Ok(then_cap.join(else_cap))
            }

            // Match: join of all arm capabilities.
            Expr::Match {
                scrutinee, arms, ..
            } => {
                let _ = self.infer_cap_tracked(ctx, scrutinee, consumed)?;
                if arms.is_empty() {
                    return Ok(Capability::Tag);
                }
                // Branch merge (same rule as `if`): a binding counts as
                // consumed after the match only if every arm consumes it.
                let base = consumed.clone();
                let mut cap = Capability::Tag;
                let mut merged: Option<FxHashSet<String>> = None;
                for (pat, guard, arm_expr) in arms {
                    *consumed = base.clone();
                    let mut arm_ctx = ctx.clone();
                    add_pat_bindings(pat, &mut arm_ctx, Capability::Val);
                    // Pattern-bound names shadow outer bindings inside the
                    // arm; hide (and restore) their outer consumption state.
                    let mut pat_names = Vec::new();
                    pat_binding_names(pat, &mut pat_names);
                    let saved: Vec<(String, bool)> = pat_names
                        .iter()
                        .map(|n| (n.clone(), consumed.remove(n)))
                        .collect();
                    // A guard runs under the same condition as the arm body,
                    // so its capability and consumption fold into the arm.
                    let guard_result = match guard {
                        Some(guard_expr) => self.infer_cap_tracked(&arm_ctx, guard_expr, consumed),
                        None => Ok(Capability::Tag),
                    };
                    let arm_result = self.infer_cap_tracked(&arm_ctx, arm_expr, consumed);
                    for (n, was_consumed) in saved {
                        consumed.remove(&n);
                        if was_consumed {
                            consumed.insert(n);
                        }
                    }
                    cap = cap.join(guard_result?.join(arm_result?));
                    merged = Some(match merged {
                        None => consumed.clone(),
                        Some(m) => m.intersection(consumed).cloned().collect(),
                    });
                }
                *consumed = merged.unwrap_or(base);
                Ok(cap)
            }

            // Block: capability of the last expression (or Unit/Val if empty).
            Expr::Block { exprs, .. } => {
                if exprs.is_empty() {
                    Ok(Capability::Val)
                } else {
                    let block_ctx = ctx.clone();
                    for (i, e) in exprs.iter().enumerate() {
                        if i == exprs.len() - 1 {
                            return self.infer_cap_tracked(&block_ctx, e, consumed);
                        }
                        // Intermediate expressions may bind variables.
                        // We don't track those for now; just infer.
                        let _ = self.infer_cap_tracked(&block_ctx, e, consumed)?;
                    }
                    Ok(Capability::Val)
                }
            }

            // Par: independence annotation, sequential block semantics.
            Expr::Par { exprs, .. } => {
                if exprs.is_empty() {
                    Ok(Capability::Val)
                } else {
                    let block_ctx = ctx.clone();
                    for (i, e) in exprs.iter().enumerate() {
                        if i == exprs.len() - 1 {
                            return self.infer_cap_tracked(&block_ctx, e, consumed);
                        }
                        // Intermediate expressions may bind variables.
                        // We don't track those for now; just infer.
                        let _ = self.infer_cap_tracked(&block_ctx, e, consumed)?;
                    }
                    Ok(Capability::Val)
                }
            }

            // Tuple: join of element capabilities.
            Expr::Tuple(elts, _) => {
                let mut cap = Capability::Val;
                for e in elts {
                    cap = cap.join(self.infer_cap_tracked(ctx, e, consumed)?);
                }
                Ok(cap)
            }

            // Record: join of field capabilities.
            Expr::Record(fields, _) => {
                let mut cap = Capability::Val;
                for (_, e) in fields {
                    cap = cap.join(self.infer_cap_tracked(ctx, e, consumed)?);
                }
                Ok(cap)
            }

            // Record update: join of base and override capabilities.
            Expr::RecordUpdate { base, fields, .. } => {
                let mut cap = self.infer_cap_tracked(ctx, base, consumed)?;
                for (_, e) in fields {
                    cap = cap.join(self.infer_cap_tracked(ctx, e, consumed)?);
                }
                Ok(cap)
            }

            // Field access: same capability as the base expression.
            Expr::FieldAccess { expr: e, .. } => self.infer_cap_tracked(ctx, e, consumed),

            // Array: join of element capabilities.
            Expr::Array(elts, _) => {
                let mut cap = Capability::Val;
                for e in elts {
                    cap = cap.join(self.infer_cap_tracked(ctx, e, consumed)?);
                }
                Ok(cap)
            }

            // Index: same capability as the array.
            Expr::Index { arr, .. } => self.infer_cap_tracked(ctx, arr, consumed),

            // Binary: join of operand capabilities.
            Expr::Binary { left, right, .. } => {
                let c1 = self.infer_cap_tracked(ctx, left, consumed)?;
                let c2 = self.infer_cap_tracked(ctx, right, consumed)?;
                Ok(c1.join(c2))
            }

            // Unary: for Ref(cap), the result has the specified capability;
            // otherwise same as operand.
            Expr::Unary { op, expr: e, .. } => {
                match op {
                    UnOp::Ref(cap) => {
                        // Unique constructors (lineariso, linear, iso, trn)
                        // MOVE the operand into the reference: a bare-variable
                        // operand is consumed exactly like `consume x`, so the
                        // value is uniquely owned afterward (`x` is unavailable
                        // on this path and a second `&iso x`/`&trn x` errors).
                        // Shared constructors (ref, val, box, tag) alias the
                        // operand without consuming it.
                        let unique = matches!(
                            cap,
                            Capability::LinearIso
                                | Capability::Linear
                                | Capability::Iso
                                | Capability::Trn
                        );
                        if unique {
                            if let Expr::Var(name, var_span) = e.as_ref() {
                                // Mark consumed regardless of capability —
                                // mirror `consume x`'s at-most-once rule.
                                self.consume_linear(name, *var_span, consumed)?;
                            } else {
                                let _ = self.infer_cap_tracked(ctx, e, consumed)?;
                            }
                        } else {
                            let _ = self.infer_cap_tracked(ctx, e, consumed)?;
                        }
                        Ok(*cap)
                    }
                    _ => self.infer_cap_tracked(ctx, e, consumed),
                }
            }

            // Assignment: returns Unit, which is Val.
            Expr::Assign { target, value, .. } => {
                let _ = self.infer_cap_tracked(ctx, target, consumed)?;
                let _ = self.infer_cap_tracked(ctx, value, consumed)?;
                Ok(Capability::Val)
            }

            // Spawn: actor references are shareable (Val).  All actor
            // is accessed through the reference itself.
            // interaction goes through message passing; nothing mutable
            Expr::Spawn {
                actor_type,
                init,
                target_node,
                ..
            } => {
                let _ = self.infer_cap_tracked(ctx, actor_type, consumed)?;
                for (_, e) in init {
                    let _ = self.infer_cap_tracked(ctx, e, consumed)?;
                }
                if let Some(node) = target_node {
                    let _ = self.infer_cap_tracked(ctx, node, consumed)?;
                }
                Ok(Capability::Val)
            }

            // Send: returns Unit (Val).  The arguments must be sendable
            // (checked separately by check_sendable).  Passing a linear
            // value as a send argument consumes it (the spec'd linear move).
            // When `remote` is true, only Val|Tag|Linear (network-serializable)
            // capabilities are accepted.
            Expr::Send {
                actor,
                args,
                remote,
                ..
            } => {
                let _ = self.infer_cap_tracked(ctx, actor, consumed)?;
                for arg in args {
                    let arg_cap = self.infer_cap_tracked(ctx, arg, consumed)?;
                    if *remote {
                        if !arg_cap.is_remote_sendable() {
                            let span = expr_span(arg);
                            self.diagnostics.push(format!(
                                "remote send argument with capability {} is not network-sendable",
                                arg_cap
                            ));
                            return Err(NuError::cap_error(format!(
                                    "remote send argument must be val, tag, or linear (serializable), got {}",
                                    arg_cap
                                ), span));
                        }
                    } else {
                        if !arg_cap.is_sendable() {
                            let span = expr_span(arg);
                            self.diagnostics.push(format!(
                                "send argument with capability {} is not sendable",
                                arg_cap
                            ));
                            return Err(NuError::cap_error(format!(
                                    "send argument must be sendable (lineariso, iso, linear, val, or tag), got {}",
                                    arg_cap
                                ), span));
                        }
                    }
                    // Consume Iso bindings on send: passing an iso value
                    // transfers ownership.  LinearIso/Linear are already
                    // consumed by Expr::Var via consume_linear.
                    if let Expr::Var(name, span) = arg {
                        if arg_cap == Capability::Iso {
                            self.consume_if_iso(name, *span, consumed)?;
                        }
                    }
                }
                Ok(Capability::Val)
            }

            // Ask: the result capability depends on what the actor returns.
            // Without type info we approximate conservatively as the join of
            // actor capability and argument capabilities.
            Expr::Ask { actor, args, .. } => {
                let mut cap = self.infer_cap_tracked(ctx, actor, consumed)?;
                for arg in args {
                    cap = cap.join(self.infer_cap_tracked(ctx, arg, consumed)?);
                    // Consume Iso bindings on ask, same as send.
                    if let Expr::Var(name, span) = arg {
                        let arg_cap = ctx.lookup(name);
                        if arg_cap == Capability::Iso {
                            self.consume_if_iso(name, *span, consumed)?;
                        }
                    }
                }
                Ok(cap)
            }

            // Receive: the capability of a receive block is the join of all
            // arm capabilities. An `after ms => body` clause evaluates `ms`
            // eagerly (outside the branch merge) and merges `body` like an
            // additional arm.
            Expr::Receive { arms, after, .. } => {
                if arms.is_empty() && after.is_none() {
                    return Ok(Capability::Tag);
                }
                // The timeout expression runs before the wait, so its
                // consumption is unconditional and part of the base state.
                if let Some((ms, _)) = after {
                    let _ = self.infer_cap_tracked(ctx, ms, consumed)?;
                }
                // Branch merge (same rule as `match`): consumed-after only if
                // every arm consumes the binding.
                let base = consumed.clone();
                let mut cap = Capability::Tag;
                let mut merged: Option<FxHashSet<String>> = None;
                for (_, patterns, guard, body_expr) in arms {
                    *consumed = base.clone();
                    let mut arm_ctx = ctx.clone();
                    // Pattern-bound names shadow outer bindings inside the
                    // arm; hide (and restore) their outer consumption state.
                    let mut pat_names: Vec<String> = Vec::new();
                    for pat in patterns {
                        add_pat_bindings(pat, &mut arm_ctx, Capability::Val);
                        pat_binding_names(pat, &mut pat_names);
                    }
                    let saved: Vec<(String, bool)> = pat_names
                        .iter()
                        .map(|n| (n.clone(), consumed.remove(n)))
                        .collect();
                    // A guard runs under the same condition as the arm body,
                    // so its capability and consumption fold into the arm.
                    let guard_result = match guard {
                        Some(guard_expr) => self.infer_cap_tracked(&arm_ctx, guard_expr, consumed),
                        None => Ok(Capability::Tag),
                    };
                    let arm_result = self.infer_cap_tracked(&arm_ctx, body_expr, consumed);
                    for (n, was_consumed) in saved {
                        consumed.remove(&n);
                        if was_consumed {
                            consumed.insert(n);
                        }
                    }
                    cap = cap.join(guard_result?.join(arm_result?));
                    merged = Some(match merged {
                        None => consumed.clone(),
                        Some(m) => m.intersection(consumed).cloned().collect(),
                    });
                }
                // Timeout arm: no pattern bindings.
                if let Some((_, body)) = after {
                    *consumed = base.clone();
                    let arm_result = self.infer_cap_tracked(ctx, body, consumed);
                    cap = cap.join(arm_result?);
                    merged = Some(match merged {
                        None => consumed.clone(),
                        Some(m) => m.intersection(consumed).cloned().collect(),
                    });
                }
                *consumed = merged.unwrap_or(base);
                Ok(cap)
            }

            // Self reference within an actor.
            Expr::SelfRef(_) => Ok(Capability::Ref),

            // Virtual actor reference: returns an actor ref (reference capability).
            Expr::GrainRef { key, .. } => {
                let _ = self.infer_cap_tracked(ctx, key, consumed)?;
                Ok(Capability::Ref)
            }

            // Perform effect: capability depends on what the operation returns.
            // Without a type environment, we join the capabilities of arguments.
            Expr::Perform { args, .. } => {
                let mut cap = Capability::Val;
                for arg in args {
                    cap = cap.join(self.infer_cap_tracked(ctx, arg, consumed)?);
                }
                Ok(cap)
            }

            // Emit event: returns Unit (Val).
            Expr::Emit { args, .. } => {
                let mut cap = Capability::Val;
                for arg in args {
                    cap = cap.join(self.infer_cap_tracked(ctx, arg, consumed)?);
                }
                Ok(cap)
            }

            // Handle: capability of the body (handlers don't change the value
            // capability, only the effect row).
            Expr::Handle { body, .. } => self.infer_cap_tracked(ctx, body, consumed),

            // Migrate: returns Unit (Val).
            Expr::Migrate { actor, node, .. } => {
                let _ = self.infer_cap_tracked(ctx, actor, consumed)?;
                let _ = self.infer_cap_tracked(ctx, node, consumed)?;
                Ok(Capability::Val)
            }

            // Explicit capability annotation.
            Expr::CapAnnotate {
                expr: inner,
                cap,
                span,
            } => {
                let inner_cap = self.infer_cap_tracked(ctx, inner, consumed)?;
                // Annotating a linear value with an aliasable capability
                // would duplicate the value; only identity and the discharge
                // target are permitted.
                //   LinearIso -> LinearIso | Iso (discharge to Iso)
                //   Linear    -> Linear    | Val (discharge to Val)
                if inner_cap.is_linear() {
                    let valid = match inner_cap {
                        Capability::LinearIso => {
                            matches!(cap, Capability::LinearIso | Capability::Iso)
                        }
                        Capability::Linear => matches!(cap, Capability::Linear | Capability::Val),
                        _ => false,
                    };
                    if !valid {
                        let msg = format!(
                            "cannot downgrade linear capability {} to {}",
                            inner_cap, cap
                        );
                        self.diagnostics.push(msg.clone());
                        return Err(NuError::cap_error(msg, *span));
                    }
                }
                Ok(*cap)
            }

            // Type annotation: capability of the inner expression.
            Expr::TypeAnnotate { expr: e, .. } => self.infer_cap_tracked(ctx, e, consumed),

            // Pipe: capability of the right-hand side applied to the left.
            Expr::Pipe { left, right, .. } => {
                let _ = self.infer_cap_tracked(ctx, left, consumed)?;
                self.infer_cap_tracked(ctx, right, consumed)
            }

            // For comprehension: capability of the body.
            Expr::For {
                var,
                iterable,
                body,
                span,
            } => {
                let _ = self.infer_cap_tracked(ctx, iterable, consumed)?;
                let body_ctx = ctx.with_binding(var.clone(), Capability::Val);
                let base = consumed.clone();
                // The loop variable shadows any outer binding of the same name.
                let outer_var = consumed.remove(var);
                let body_result = self.infer_cap_tracked(&body_ctx, body, consumed);
                consumed.remove(var);
                if outer_var {
                    consumed.insert(var.clone());
                }
                let body_cap = body_result?;
                // A loop body may execute more than once, so consuming an
                // outer linear binding inside the body could use it multiple
                // times along a single path — reject it outright.
                if let Some(name) = consumed.difference(&base).next() {
                    let name = name.clone();
                    let msg = format!(
                        "linear value `{}` consumed in loop body may be used more than once",
                        name
                    );
                    self.diagnostics.push(msg.clone());
                    return Err(NuError::cap_error(msg, *span));
                }
                // The loop may also execute zero times, so body consumption
                // does not propagate past the loop.
                *consumed = base;
                Ok(body_cap)
            }

            // While loop: capability of body; cond is read-only.
            Expr::While { cond, body, span } => {
                let _ = self.infer_cap_tracked(ctx, cond, consumed)?;
                let base = consumed.clone();
                let body_result = self.infer_cap_tracked(ctx, body, consumed);
                let body_cap = body_result?;
                if let Some(name) = consumed.difference(&base).next() {
                    let name = name.clone();
                    let msg = format!(
                        "linear value `{}` consumed in loop body may be used more than once",
                        name
                    );
                    self.diagnostics.push(msg.clone());
                    return Err(NuError::cap_error(msg, *span));
                }
                *consumed = base;
                Ok(body_cap)
            }
            // Return: capability of returned value.
            Expr::Return(Some(e), _) => self.infer_cap_tracked(ctx, e, consumed),
            Expr::Return(None, _) => Ok(Capability::Val),

            // Break: never returns a value, use Tag.
            Expr::Break(..) => Ok(Capability::Tag),

            // Consume: mark the variable as consumed, return its capability.
            Expr::Consume {
                expr: inner,
                span: _,
            } => {
                // If consuming a variable, mark it as consumed in the linear tracker.
                if let Expr::Var(name, var_span) = inner.as_ref() {
                    // Mark consumed regardless of capability — consume x
                    // means x is unavailable after this point.
                    self.consume_linear(name, *var_span, consumed)?;
                    Ok(ctx.lookup(name))
                } else {
                    // For non-variable expressions, just infer capability.
                    self.infer_cap_tracked(ctx, inner, consumed)
                }
            }

            // Recover: isolated scope — the body must produce a sendable result.
            Expr::Recover { body, span } => {
                let body_cap = self.infer_cap_tracked(ctx, body, consumed)?;
                if !body_cap.is_sendable() {
                    let msg = format!(
                        "recover body must evaluate to a sendable value, but got {}",
                        body_cap
                    );
                    self.diagnostics.push(msg.clone());
                    return Err(NuError::cap_error(msg, *span));
                }
                Ok(body_cap)
            }
            // Defer: capability of the deferred expression.
            Expr::Defer { expr, .. } => self.infer_cap_tracked(ctx, expr, consumed),
            Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
                self.infer_cap_tracked(ctx, body, consumed)
            }
            Expr::Panic(..) => Ok(Capability::Val),
            Expr::Resume { value, .. } => {
                let _ = self.infer_cap_tracked(ctx, value, consumed)?;
                Ok(Capability::Val)
            }
        }
    }

    /// Check that a capability is a subtype of another.
    ///
    /// Returns `Ok(())` if `sub <: sup`, otherwise emits a [`NuError::CapError`].
    pub fn check_cap_sub(&mut self, sub: Capability, sup: Capability, span: Span) -> NuResult<()> {
        if sub.is_subtype_of(sup) {
            Ok(())
        } else {
            let mut msg = format!("capability {} is not a subtype of {}", sub, sup);
            if sub == Capability::LinearIso && sup == Capability::Iso {
                msg.push_str("\nhelp: use `consume <binding>` to discharge the linear obligation and obtain an Iso reference");
            }
            self.diagnostics.push(msg.clone());
            Err(NuError::cap_error_explained(
                msg,
                span,
                format!(
                    "capability `{}` does not satisfy the subtyping relation `{} <: {}`",
                    sub, sub, sup
                ),
            ))
        }
    }

    /// Check that a capability is sendable (can cross an actor boundary).
    ///
    /// Sendable capabilities are `LinearIso`, `Iso`, `Linear`, `Val`, and `Tag`.
    pub fn check_sendable(&mut self, cap: Capability, span: Span) -> NuResult<()> {
        if cap.is_sendable() {
            Ok(())
        } else {
            let msg = format!(
                "capability {} is not sendable (must be lineariso, iso, linear, val, or tag)",
                cap
            );
            self.diagnostics.push(msg.clone());
            Err(NuError::cap_error_explained(
                msg,
                span,
                format!(
                    "only lineariso, iso, linear, val, and tag may cross an actor boundary; `{}` cannot",
                    cap
                ),
            ))
        }
    }

    /// Check sendability of an expression's result.
    ///
    /// Infers the expression's capability and then checks that it is sendable.
    pub fn check_expr_sendable(&mut self, ctx: &CapContext, expr: &Expr) -> NuResult<()> {
        let cap = self.infer_cap(ctx, expr)?;
        let span = expr_span(expr);
        self.check_sendable(cap, span)
    }
}

impl Default for CapabilityAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the source span from any expression node.
fn expr_span(expr: &Expr) -> Span {
    match expr {
        Expr::Literal(_, s) => *s,
        Expr::FString(_, s) => *s,
        Expr::Var(_, s) => *s,
        Expr::Lambda { span, .. } => *span,
        Expr::App { span, .. } => *span,
        Expr::Let { span, .. } => *span,
        Expr::LetRec { span, .. } => *span,
        Expr::If { span, .. } => *span,
        Expr::Match { span, .. } => *span,
        Expr::Block { span, .. } => *span,
        Expr::Par { span, .. } => *span,
        Expr::Tuple(_, s) => *s,
        Expr::Record(_, s) => *s,
        Expr::FieldAccess { span, .. } => *span,
        Expr::Array(_, s) => *s,
        Expr::Index { span, .. } => *span,
        Expr::RecordUpdate { span, .. } => *span,
        Expr::Binary { span, .. } => *span,
        Expr::Unary { span, .. } => *span,
        Expr::Assign { span, .. } => *span,
        Expr::Spawn { span, .. } => *span,
        Expr::Send { span, .. } => *span,
        Expr::Ask { span, .. } => *span,
        Expr::Receive { span, .. } => *span,
        Expr::SelfRef(s) => *s,
        Expr::GrainRef { span, .. } => *span,
        Expr::Emit { span, .. } => *span,
        Expr::Perform { span, .. } => *span,
        Expr::Handle { span, .. } => *span,
        Expr::Migrate { span, .. } => *span,
        Expr::CapAnnotate { span, .. } => *span,
        Expr::TypeAnnotate { span, .. } => *span,
        Expr::Pipe { span, .. } => *span,
        Expr::For { span, .. } => *span,
        Expr::While { span, .. } => *span,
        Expr::Return(_, s) => *s,
        Expr::Break(_, s) => *s,
        Expr::Consume { span, .. } => *span,
        Expr::Recover { span, .. } => *span,
        Expr::Defer { span, .. } => *span,
        Expr::Hide { span, .. } | Expr::Seal { span, .. } => *span,
        Expr::Panic(_, span) => *span,
        Expr::Resume { span, .. } => *span,
    }
}

/// Format an effect row for diagnostic messages.
fn format_row(row: &EffectRow) -> String {
    format!("{}", row)
}

/// Add pattern-bound variables to the capability context with a given
/// default capability.
fn add_pat_bindings(pat: &Pattern, ctx: &mut CapContext, cap: Capability) {
    match pat {
        Pattern::Wild => {}
        Pattern::Var(name) | Pattern::Alias(name, _) => {
            ctx.bindings.push((name.clone(), cap));
        }
        Pattern::Lit(_) => {}
        Pattern::Tuple(pats) => {
            for p in pats {
                add_pat_bindings(p, ctx, cap);
            }
        }
        Pattern::Record(fields) => {
            for (_, p) in fields {
                add_pat_bindings(p, ctx, cap);
            }
        }
        Pattern::Variant(_, Some(inner)) => {
            add_pat_bindings(inner, ctx, cap);
        }
        Pattern::Variant(_, None) => {}
    }
}

/// Collect the variable names bound by a pattern (for scope/shadowing
/// bookkeeping in the linear-consumption tracker).
fn pat_binding_names(pat: &Pattern, acc: &mut Vec<String>) {
    match pat {
        Pattern::Wild | Pattern::Lit(_) => {}
        Pattern::Var(name) | Pattern::Alias(name, _) => acc.push(name.clone()),
        Pattern::Tuple(pats) => {
            for p in pats {
                pat_binding_names(p, acc);
            }
        }
        Pattern::Record(fields) => {
            for (_, p) in fields {
                pat_binding_names(p, acc);
            }
        }
        Pattern::Variant(_, Some(inner)) => pat_binding_names(inner, acc),
        Pattern::Variant(_, None) => {}
    }
}

// ---------------------------------------------------------------------------
// Single-shot linearity analysis for effect handlers
// ---------------------------------------------------------------------------

/// Determine whether a handler body is *single-shot*: the continuation is
/// consumed at most once on every control-flow path.  When `true` the VM can
/// skip heap-allocating a `Continuation` and use a lightweight inline path.
///
/// In Nulang the `resume` keyword on a handler arm (`| E.op() resume => body`)
/// is syntactic sugar — there is no explicit `resume(…)` call.  The body's
/// final value is *implicitly* resumed.  A resuming handler body therefore
/// has exactly one resume point (the body terminator) unless the body
/// contains a loop whose interior might indirectly trigger another resume
/// (e.g. via a nested `perform`).
///
/// The analysis is conservative:
/// - Non-resuming handlers are trivially single-shot (zero resumes).
/// - Resuming handlers are single-shot iff the body contains no `While`/`For`
///   loops and no function/routine calls that might resume transitively.
/// - `If`/`Match` branches are checked individually; having two branches both
///   end in a resume is fine — only one branch executes per invocation.
pub fn is_single_shot(body: &crate::hir::Body, resume: bool) -> bool {
    if !resume {
        return true; // zero-shot: the continuation is never invoked
    }
    body_is_single_shot(body)
}

fn body_is_single_shot(body: &crate::hir::Body) -> bool {
    for stmt in &body.stmts {
        if !stmt_is_single_shot(stmt) {
            return false;
        }
    }
    // The terminator is always an implicit resume for resuming handlers —
    // that's the single shot.  No further check needed at the terminator.
    true
}

fn stmt_is_single_shot(stmt: &crate::hir::Stmt) -> bool {
    match stmt {
        crate::hir::Stmt::Let { value, .. } | crate::hir::Stmt::Assign { value, .. } => {
            rvalue_is_single_shot(value)
        }
        crate::hir::Stmt::StateSet { .. } | crate::hir::Stmt::Emit { .. } => true,
    }
}

fn rvalue_is_single_shot(rv: &crate::hir::RValue) -> bool {
    match rv {
        // Straight-line leaf values are fine.
        crate::hir::RValue::Use(_)
        | crate::hir::RValue::Literal(..)
        | crate::hir::RValue::Binary(..)
        | crate::hir::RValue::Unary(..)
        | crate::hir::RValue::Tuple(..)
        | crate::hir::RValue::Record(..)
        | crate::hir::RValue::RecordUpdate { .. }
        | crate::hir::RValue::Array(..)
        | crate::hir::RValue::FieldAccess { .. }
        | crate::hir::RValue::Index { .. }
        | crate::hir::RValue::SelfRef(_)
        | crate::hir::RValue::Panic(_)
        | crate::hir::RValue::CapCheck { .. } => true,

        // Function calls — conservative: a callee might perform an effect
        // that is handled by an outer handler, causing a second resume
        // into this handler body.
        crate::hir::RValue::Call { .. }
        | crate::hir::RValue::Closure { .. }
        | crate::hir::RValue::RecClosure { .. } => false,

        // Scoped block: check the inner body.
        crate::hir::RValue::Block(body) => body_is_single_shot(body),

        // Branches: check each arm.  Both branches ending in a resume is
        // fine — only one executes.
        crate::hir::RValue::If {
            then_body,
            else_body,
            ..
        } => {
            body_is_single_shot(then_body)
                && else_body.as_ref().map_or(true, |b| body_is_single_shot(b))
        }
        crate::hir::RValue::Match { arms, .. } => arms.iter().all(|(_, guard, arm_body)| {
            body_is_single_shot(arm_body) && guard.as_ref().map_or(true, |g| body_is_single_shot(g))
        }),

        // Loops: the loop body could execute multiple times, and each
        // iteration might trigger an indirect resume via a nested
        // `perform`.  Conservative: not single-shot.
        crate::hir::RValue::While { .. } | crate::hir::RValue::For { .. } => false,

        // Nested `handle`: check the body and each handler body.
        crate::hir::RValue::Handle { body, handlers, .. } => {
            body_is_single_shot(body) && handlers.iter().all(|h| is_single_shot(&h.body, h.resume))
        }

        // `perform` and `receive` are straight-line operations from the
        // handler body's perspective — they don't resume into *this*
        // handler body multiple times.
        crate::hir::RValue::Perform { .. }
        | crate::hir::RValue::Receive { .. }
        | crate::hir::RValue::Spawn { .. }
        | crate::hir::RValue::Send { .. }
        | crate::hir::RValue::Ask { .. }
        | crate::hir::RValue::Migrate { .. }
        | crate::hir::RValue::FFICall { .. }
        | crate::hir::RValue::PipelineNew { .. }
        | crate::hir::RValue::PipelineStage { .. }
        | crate::hir::RValue::PipelineRun { .. }
        | crate::hir::RValue::SupervisorNew { .. }
        | crate::hir::RValue::SupervisorWorker { .. }
        | crate::hir::RValue::SupervisorRun { .. }
        | crate::hir::RValue::DebateNew { .. }
        | crate::hir::RValue::DebateParticipant { .. }
        | crate::hir::RValue::DebateRun { .. } => true,
        crate::hir::RValue::Resume { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a zero span.
    fn s() -> Span {
        Span::default()
    }

    // -----------------------------------------------------------------------
    // Effect row operation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_effect_row_subset_closed() {
        let a = EffectRow::Closed(vec![Effect::IO, Effect::FS]);
        let b = EffectRow::Closed(vec![Effect::IO, Effect::FS, Effect::Net]);
        assert!(effect_row_subset(&a, &b)); // {IO, FS} ⊆ {IO, FS, Net}
        assert!(!effect_row_subset(&b, &a)); // {IO, FS, Net} ⊄ {IO, FS}
    }

    #[test]
    fn test_effect_row_subset_empty() {
        let empty = EffectRow::empty();
        let row = EffectRow::Closed(vec![Effect::IO]);
        assert!(effect_row_subset(&empty, &row)); // {} ⊆ {IO}
        assert!(effect_row_subset(&empty, &empty)); // {} ⊆ {}
        assert!(!effect_row_subset(&row, &empty)); // {IO} ⊄ {}
    }

    #[test]
    fn test_effect_row_subset_open() {
        let closed = EffectRow::Closed(vec![Effect::IO]);
        let open = EffectRow::Open(vec![Effect::IO], Region::fresh());
        assert!(effect_row_subset(&closed, &open));
    }

    #[test]
    fn test_effect_row_union() {
        let a = EffectRow::Closed(vec![Effect::IO]);
        let b = EffectRow::Closed(vec![Effect::FS]);
        let u = effect_row_union(&a, &b);
        assert!(u.contains(&Effect::IO));
        assert!(u.contains(&Effect::FS));
    }

    #[test]
    fn test_effect_row_union_dedup() {
        let a = EffectRow::Closed(vec![Effect::IO, Effect::FS]);
        let b = EffectRow::Closed(vec![Effect::FS, Effect::Net]);
        let u = effect_row_union(&a, &b);
        // Both IO and FS and Net should be present.
        assert!(u.contains(&Effect::IO));
        assert!(u.contains(&Effect::FS));
        assert!(u.contains(&Effect::Net));
    }

    #[test]
    fn test_effect_row_diff() {
        let row = EffectRow::Closed(vec![Effect::IO, Effect::FS, Effect::Net]);
        let diff = effect_row_diff(&row, &Effect::FS);
        assert!(diff.contains(&Effect::IO));
        assert!(!diff.contains(&Effect::FS));
        assert!(diff.contains(&Effect::Net));
    }

    #[test]
    fn test_effect_row_diff_open() {
        let row = EffectRow::Open(vec![Effect::IO, Effect::FS], Region::fresh());
        let diff = effect_row_diff(&row, &Effect::FS);
        assert!(diff.contains(&Effect::IO));
        assert!(!diff.contains(&Effect::FS));
    }

    // -----------------------------------------------------------------------
    // Effect parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_effect_name_builtin() {
        assert_eq!(parse_effect_name("IO"), Effect::IO);
        assert_eq!(parse_effect_name("Net"), Effect::Net);
        assert_eq!(parse_effect_name("FS"), Effect::FS);
        assert_eq!(parse_effect_name("Test"), Effect::Test);
        assert_eq!(parse_effect_name("Spawn"), Effect::Spawn);
        assert_eq!(parse_effect_name("Http"), Effect::Net);
        assert_eq!(parse_effect_name("Async"), Effect::Async);
        assert_eq!(parse_effect_name("Inference"), Effect::Inference);
    }

    #[test]
    fn test_parse_effect_name_user_defined() {
        assert_eq!(
            parse_effect_name("MyEffect"),
            Effect::UserDefined("MyEffect".to_string())
        );
    }

    #[test]
    fn test_parse_effect_name_ffi() {
        assert_eq!(parse_effect_name("FFI"), Effect::FFI);
    }

    // -----------------------------------------------------------------------
    // Effect inference tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_literal_is_pure() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let lit = Expr::Literal(Literal::Int(42), s());
        let row = checker.infer_effects(&ctx, &lit).unwrap();
        assert_eq!(row, EffectRow::empty());
    }

    #[test]
    fn test_infer_var_is_pure() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let var = Expr::Var("x".to_string(), s());
        let row = checker.infer_effects(&ctx, &var).unwrap();
        assert_eq!(row, EffectRow::empty());
    }

    #[test]
    fn test_infer_lambda_is_pure() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let lam = Expr::Lambda {
            params: vec![Param::new("x", None)],
            ret_type: None,
            body: Box::new(Expr::Var("x".to_string(), s())),
            effect: None,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &lam).unwrap();
        assert_eq!(row, EffectRow::empty());
    }

    #[test]
    fn test_infer_perform_io() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let perform = Expr::Perform {
            effect: "IO".to_string(),
            op: "print".to_string(),
            args: vec![Expr::Literal(Literal::String("hello".to_string()), s())],
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &perform).unwrap();
        assert!(row.contains(&Effect::IO));
        assert!(!row.contains(&Effect::FS));
    }

    #[test]
    fn test_infer_perform_fs() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let perform = Expr::Perform {
            effect: "FS".to_string(),
            op: "read".to_string(),
            args: vec![Expr::Literal(
                Literal::String("/tmp/test.txt".to_string()),
                s(),
            )],
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &perform).unwrap();
        assert!(row.contains(&Effect::FS));
        assert!(!row.contains(&Effect::IO));
    }

    #[test]
    fn test_infer_spawn_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let spawn = Expr::Spawn {
            actor_type: Box::new(Expr::Var("MyActor".to_string(), s())),
            init: vec![],
            positional_args: None,
            register_as: None,
            target_node: None,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &spawn).unwrap();
        assert!(row.contains(&Effect::Spawn));
    }

    #[test]
    fn test_infer_send_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let send = Expr::Send {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![Expr::Literal(Literal::Int(1), s())],
            remote: false,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &send).unwrap();
        assert!(row.contains(&Effect::Send));
    }

    #[test]
    fn test_infer_ask_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let ask = Expr::Ask {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![],
            remote: false,
            timeout_ms: None,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &ask).unwrap();
        assert!(row.contains(&Effect::Send));
        assert!(row.contains(&Effect::Receive));
    }

    #[test]
    fn test_infer_let_combines_effects() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let let_expr = Expr::Let {
            name: "x".to_string(),
            ty: None,
            value: Box::new(Expr::Perform {
                effect: "FS".to_string(),
                op: "read".to_string(),
                args: vec![],
                span: s(),
            }),
            body: Box::new(Expr::Perform {
                effect: "Net".to_string(),
                op: "get".to_string(),
                args: vec![],
                span: s(),
            }),
            mutable: false,
            let_in: false,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &let_expr).unwrap();
        assert!(row.contains(&Effect::FS));
        assert!(row.contains(&Effect::Net));
    }

    #[test]
    fn test_infer_if_combines_effects() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let if_expr = Expr::If {
            cond: Box::new(Expr::Literal(Literal::Bool(true), s())),
            then_branch: Box::new(Expr::Perform {
                effect: "IO".to_string(),
                op: "print".to_string(),
                args: vec![],
                span: s(),
            }),
            else_branch: Some(Box::new(Expr::Perform {
                effect: "FS".to_string(),
                op: "read".to_string(),
                args: vec![],
                span: s(),
            })),
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &if_expr).unwrap();
        assert!(row.contains(&Effect::IO));
        assert!(row.contains(&Effect::FS));
    }

    #[test]
    fn test_infer_handle_removes_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let handle_expr = Expr::Handle {
            body: Box::new(Expr::Perform {
                effect: "IO".to_string(),
                op: "print".to_string(),
                args: vec![Expr::Literal(Literal::String("hi".to_string()), s())],
                span: s(),
            }),
            handlers: vec![EffectHandler {
                effect_name: "IO".to_string(),
                op_name: "print".to_string(),
                params: vec!["msg".to_string()],
                body: Expr::Literal(Literal::Unit, s()),
                resume: false,
            }],
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &handle_expr).unwrap();
        // The IO effect should be handled (removed from the body row).
        assert!(!row.contains(&Effect::IO));
    }

    #[test]
    fn test_check_effects_passes() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::with_allowed(EffectRow::Closed(vec![Effect::IO, Effect::FS]));
        let expr = Expr::Perform {
            effect: "IO".to_string(),
            op: "print".to_string(),
            args: vec![],
            span: s(),
        };
        assert!(checker
            .check_effects(&ctx, &expr, &ctx.allowed_effects)
            .is_ok());
    }

    #[test]
    fn test_check_effects_fails() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::with_allowed(EffectRow::Closed(vec![Effect::IO]));
        let expr = Expr::Perform {
            effect: "FS".to_string(),
            op: "read".to_string(),
            args: vec![],
            span: Span::new(0, 10),
        };
        let result = checker.check_effects(&ctx, &expr, &ctx.allowed_effects);
        assert!(result.is_err());
        match result.unwrap_err() {
            NuError::EffectError { msg, .. } => {
                assert!(
                    msg.contains("FS"),
                    "error message should mention FS: {}",
                    msg
                );
            }
            other => panic!("expected EffectError, got {:?}", other),
        }
    }

    #[test]
    fn test_perform_empty_op_name_errors() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let perform = Expr::Perform {
            effect: "IO".to_string(),
            op: "".to_string(),
            args: vec![],
            span: s(),
        };
        let result = checker.infer_effects(&ctx, &perform);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Capability analysis tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cap_literal_is_val() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let lit = Expr::Literal(Literal::Int(42), s());
        let cap = analyzer.infer_cap(&ctx, &lit).unwrap();
        assert_eq!(cap, Capability::Val);
    }

    #[test]
    fn test_cap_var_lookup() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Iso);
        let var = Expr::Var("x".to_string(), s());
        let cap = analyzer.infer_cap(&ctx, &var).unwrap();
        assert_eq!(cap, Capability::Iso);
    }

    #[test]
    fn test_cap_var_default() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let var = Expr::Var("unknown".to_string(), s());
        let cap = analyzer.infer_cap(&ctx, &var).unwrap();
        assert_eq!(cap, Capability::Val); // default
    }

    #[test]
    fn test_cap_lambda_no_captures() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let lam = Expr::Lambda {
            params: vec![Param::new("x", None)],
            ret_type: None,
            body: Box::new(Expr::Var("x".to_string(), s())),
            effect: None,
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &lam).unwrap();
        assert_eq!(cap, Capability::Val);
    }

    #[test]
    fn test_cap_lambda_with_capture() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("y", Capability::Ref);
        let lam = Expr::Lambda {
            params: vec![Param::new("x", None)],
            ret_type: None,
            body: Box::new(Expr::Binary {
                op: BinOp::Add,
                left: Box::new(Expr::Var("x".to_string(), s())),
                right: Box::new(Expr::Var("y".to_string(), s())),
                span: s(),
            }),
            effect: None,
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &lam).unwrap();
        assert_eq!(cap, Capability::Ref);
    }

    #[test]
    fn test_cap_spawn_is_val() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let spawn = Expr::Spawn {
            actor_type: Box::new(Expr::Var("A".to_string(), s())),
            init: vec![],
            positional_args: None,
            register_as: None,
            target_node: None,
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &spawn).unwrap();
        assert_eq!(cap, Capability::Val);
    }

    #[test]
    fn test_cap_annotate() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = Expr::CapAnnotate {
            expr: Box::new(Expr::Literal(Literal::Int(1), s())),
            cap: Capability::Iso,
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &expr).unwrap();
        assert_eq!(cap, Capability::Iso);
    }

    #[test]
    fn test_check_cap_sub_passes() {
        let mut analyzer = CapabilityAnalyzer::new();
        // Val <: Box (val can be read as box)
        assert!(analyzer
            .check_cap_sub(Capability::Val, Capability::Box, s())
            .is_ok());
        // Tag <: Iso (tag is bottom of the lattice)
        assert!(analyzer
            .check_cap_sub(Capability::Tag, Capability::Iso, s())
            .is_ok());
        // Ref <: Box (ref can be read as box)
        assert!(analyzer
            .check_cap_sub(Capability::Ref, Capability::Box, s())
            .is_ok());
    }

    #[test]
    fn test_check_cap_sub_fails() {
        let mut analyzer = CapabilityAnalyzer::new();
        let result = analyzer.check_cap_sub(Capability::Ref, Capability::Val, s());
        assert!(result.is_err());
    }

    #[test]
    fn test_check_sendable_passes() {
        let mut analyzer = CapabilityAnalyzer::new();
        assert!(analyzer.check_sendable(Capability::LinearIso, s()).is_ok());
        assert!(analyzer.check_sendable(Capability::Iso, s()).is_ok());
        assert!(analyzer.check_sendable(Capability::Linear, s()).is_ok());
        assert!(analyzer.check_sendable(Capability::Val, s()).is_ok());
        assert!(analyzer.check_sendable(Capability::Tag, s()).is_ok());
    }

    #[test]
    fn test_check_sendable_fails() {
        let mut analyzer = CapabilityAnalyzer::new();
        assert!(analyzer.check_sendable(Capability::Ref, s()).is_err());
        assert!(analyzer.check_sendable(Capability::Box, s()).is_err());
    }

    #[test]
    fn test_cap_ref_creation() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = Expr::Unary {
            op: UnOp::Ref(Capability::Iso),
            expr: Box::new(Expr::Literal(Literal::Int(42), s())),
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &expr).unwrap();
        assert_eq!(cap, Capability::Iso);
    }

    #[test]
    fn test_cap_ref_creation_all_caps() {
        // Every value-level `&cap` constructor yields a reference with the
        // requested capability.
        for (cap, want) in [
            (Capability::Ref, Capability::Ref),
            (Capability::Iso, Capability::Iso),
            (Capability::Trn, Capability::Trn),
            (Capability::Val, Capability::Val),
            (Capability::Box, Capability::Box),
            (Capability::Tag, Capability::Tag),
            (Capability::LinearIso, Capability::LinearIso),
            (Capability::Linear, Capability::Linear),
        ] {
            let mut analyzer = CapabilityAnalyzer::new();
            let ctx = CapContext::new();
            let expr = Expr::Unary {
                op: UnOp::Ref(cap),
                expr: Box::new(Expr::Literal(Literal::Int(42), s())),
                span: s(),
            };
            let got = analyzer.infer_cap(&ctx, &expr).unwrap();
            assert_eq!(got, want, "cap {cap}");
        }
    }

    #[test]
    fn test_cap_unique_ref_consumes_operand() {
        // Unique constructors (iso/trn/lineariso/linear) MOVE a bare
        // variable operand: a second `&iso x` on the same binding must
        // fail, exactly like a second `consume x`.
        for cap in [
            Capability::Iso,
            Capability::Trn,
            Capability::LinearIso,
            Capability::Linear,
        ] {
            let mut analyzer = CapabilityAnalyzer::new();
            let ctx = CapContext::new().with_binding("x", Capability::Val);
            let expr = Expr::Block {
                exprs: vec![
                    Expr::Unary {
                        op: UnOp::Ref(cap),
                        expr: Box::new(lvar("x")),
                        span: s(),
                    },
                    Expr::Unary {
                        op: UnOp::Ref(cap),
                        expr: Box::new(lvar("x")),
                        span: s(),
                    },
                ],
                span: s(),
            };
            let result = analyzer.infer_cap(&ctx, &expr);
            assert!(
                result.is_err(),
                "double {cap} reference must be rejected, got {:?}",
                result
            );
        }
    }

    #[test]
    fn test_cap_shared_ref_does_not_consume_operand() {
        // Shared constructors (ref/val/box/tag) alias without consuming:
        // repeated construction from the same binding is fine.
        for cap in [
            Capability::Ref,
            Capability::Val,
            Capability::Box,
            Capability::Tag,
        ] {
            let mut analyzer = CapabilityAnalyzer::new();
            let ctx = CapContext::new().with_binding("x", Capability::Val);
            let expr = Expr::Block {
                exprs: vec![
                    Expr::Unary {
                        op: UnOp::Ref(cap),
                        expr: Box::new(lvar("x")),
                        span: s(),
                    },
                    Expr::Unary {
                        op: UnOp::Ref(cap),
                        expr: Box::new(lvar("x")),
                        span: s(),
                    },
                ],
                span: s(),
            };
            analyzer
                .infer_cap(&ctx, &expr)
                .unwrap_or_else(|e| panic!("shared {cap} constructor must pass: {e}"));
        }
    }

    #[test]
    fn test_cap_binary_join() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        // A binary expression where we just need to check the join works.
        let expr = Expr::Binary {
            op: BinOp::Add,
            left: Box::new(Expr::Literal(Literal::Int(1), s())),
            right: Box::new(Expr::Literal(Literal::Int(2), s())),
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &expr).unwrap();
        // Val join Val = Val
        assert_eq!(cap, Capability::Val);
    }

    #[test]
    fn test_cap_send_checks_sendable() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("a", Capability::Iso);
        // Send with a non-sendable argument should fail.
        let send = Expr::Send {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![Expr::Var("ref_var".to_string(), s())],
            remote: false,
            span: s(),
        };
        // ref_var defaults to Val (sendable), so it passes. Let's test with
        // a non-sendable binding.
        let ctx2 = ctx.with_binding("ref_var", Capability::Ref);
        let result = analyzer.infer_cap(&ctx2, &send);
        assert!(result.is_err(), "send with ref argument should fail");
    }

    #[test]
    fn test_cap_remote_send_rejects_iso() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::Iso);
        let send = Expr::Send {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![Expr::Var("x".to_string(), s())],
            remote: true,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &send);
        assert!(result.is_err(), "remote send with iso argument should fail");
    }

    #[test]
    fn test_cap_remote_send_accepts_val() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::Val);
        let send = Expr::Send {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![Expr::Var("x".to_string(), s())],
            remote: true,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &send);
        assert!(result.is_ok(), "remote send with val argument should pass");
    }

    #[test]
    fn test_cap_remote_send_accepts_tag() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::Tag);
        let send = Expr::Send {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            behavior: "foo".to_string(),
            args: vec![Expr::Var("x".to_string(), s())],
            remote: true,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &send);
        assert!(result.is_ok(), "remote send with tag argument should pass");
    }

    #[test]
    fn test_cap_self_ref_is_ref() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = Expr::SelfRef(s());
        let cap = analyzer.infer_cap(&ctx, &expr).unwrap();
        assert_eq!(cap, Capability::Ref);
    }

    #[test]
    fn test_cap_break_is_tag() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = Expr::Break(None, s());
        let cap = analyzer.infer_cap(&ctx, &expr).unwrap();
        assert_eq!(cap, Capability::Tag);
    }

    #[test]
    fn test_effect_context_with_handler() {
        let ctx = EffectContext::with_allowed(EffectRow::Closed(vec![Effect::IO]));
        let ctx2 = ctx.with_handler(Effect::IO);
        assert_eq!(ctx2.handlers.len(), 1);
        assert!(ctx2.handlers.contains(&Effect::IO));
    }

    #[test]
    fn test_cap_context_lookup_and_binding() {
        let ctx = CapContext::new().with_binding("x", Capability::Iso);
        assert_eq!(ctx.lookup("x"), Capability::Iso);
        assert_eq!(ctx.lookup("unknown"), Capability::Val); // default

        let ctx2 = ctx.with_binding("y", Capability::Ref);
        assert_eq!(ctx2.lookup("y"), Capability::Ref);
        assert_eq!(ctx2.lookup("x"), Capability::Iso);
    }

    #[test]
    fn test_cap_context_with_params_preserves_only_annotations() {
        let params = vec![
            Param::new("plain", None),
            Param::new("owned", None).with_cap(Capability::LinearIso),
        ];
        let ctx = CapContext::new().with_params(&params);
        assert_eq!(ctx.lookup("plain"), Capability::Val);
        assert_eq!(ctx.lookup("owned"), Capability::LinearIso);
    }

    #[test]
    fn test_infer_migrate_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let migrate = Expr::Migrate {
            actor: Box::new(Expr::Var("a".to_string(), s())),
            node: Box::new(Expr::Literal(Literal::String("node1".to_string()), s())),
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &migrate).unwrap();
        assert!(row.contains(&Effect::Migrate));
    }

    #[test]
    fn test_infer_receive_effect() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let receive = Expr::Receive {
            arms: vec![(
                "Msg".to_string(),
                vec![Pattern::Var("x".to_string())],
                None,
                Expr::Var("x".to_string(), s()),
            )],
            after: None,
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &receive).unwrap();
        assert!(row.contains(&Effect::Receive));
    }

    #[test]
    fn test_infer_receive_after_effect() {
        // receive { | Msg() => 0 } after 100 => perform Logger.log("t"):
        // the after clause contributes its body's effects to the row.
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let receive = Expr::Receive {
            arms: vec![(
                "Msg".to_string(),
                vec![],
                None,
                Expr::Literal(Literal::Int(0), s()),
            )],
            after: Some((
                Box::new(Expr::Literal(Literal::Int(100), s())),
                Box::new(Expr::Perform {
                    effect: "Logger".to_string(),
                    op: "log".to_string(),
                    args: vec![Expr::Literal(Literal::String("t".to_string()), s())],
                    span: s(),
                }),
            )),
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &receive).unwrap();
        assert!(row.contains(&Effect::Receive));
        assert!(row.contains(&Effect::UserDefined("Logger".to_string())));
    }

    #[test]
    fn test_infer_perform_user_defined() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let perform = Expr::Perform {
            effect: "Logger".to_string(),
            op: "log".to_string(),
            args: vec![Expr::Literal(Literal::String("msg".to_string()), s())],
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &perform).unwrap();
        assert!(row.contains(&Effect::UserDefined("Logger".to_string())));
    }

    #[test]
    fn test_infer_lambda_effect_annotation_satisfied() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let lam = Expr::Lambda {
            params: vec![Param::new("x", None)],
            ret_type: None,
            body: Box::new(Expr::Perform {
                effect: "IO".to_string(),
                op: "print".to_string(),
                args: vec![Expr::Var("x".to_string(), s())],
                span: s(),
            }),
            effect: Some(EffectRow::Closed(vec![Effect::IO])),
            span: s(),
        };
        let row = checker.infer_effects(&ctx, &lam).unwrap();
        assert_eq!(row, EffectRow::Closed(vec![Effect::IO]));
    }

    #[test]
    fn test_infer_lambda_effect_annotation_violated() {
        let mut checker = EffectChecker::new();
        let ctx = EffectContext::empty();
        let lam = Expr::Lambda {
            params: vec![Param::new("x", None)],
            ret_type: None,
            body: Box::new(Expr::Perform {
                effect: "FS".to_string(),
                op: "read".to_string(),
                args: vec![],
                span: s(),
            }),
            effect: Some(EffectRow::Closed(vec![Effect::IO])),
            span: s(),
        };
        let result = checker.infer_effects(&ctx, &lam);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // LinearIso consumption tracking (at-most-once use)
    // -----------------------------------------------------------------------

    // Helpers for building linearity test expressions.
    fn lvar(name: &str) -> Expr {
        Expr::Var(name.to_string(), s())
    }

    fn call1(func: &str, arg: Expr) -> Expr {
        Expr::App {
            func: Box::new(Expr::Var(func.to_string(), s())),
            args: vec![arg],
            span: s(),
        }
    }

    fn send_m(arg: Expr) -> Expr {
        Expr::Send {
            actor: Box::new(lvar("a")),
            behavior: "m".to_string(),
            args: vec![arg],
            remote: false,
            span: s(),
        }
    }

    fn let_expr(name: &str, value: Expr, body: Expr) -> Expr {
        Expr::Let {
            name: name.to_string(),
            ty: None,
            value: Box::new(value),
            body: Box::new(body),
            mutable: false,
            let_in: true,
            span: s(),
        }
    }

    fn fresh_lineariso(n: i64) -> Expr {
        Expr::CapAnnotate {
            expr: Box::new(Expr::Literal(Literal::Int(n), s())),
            cap: Capability::LinearIso,
            span: s(),
        }
    }

    #[test]
    fn test_lineariso_used_once_ok() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let cap = analyzer.infer_cap(&ctx, &call1("f", lvar("x"))).unwrap();
        assert_eq!(cap, Capability::Val); // Val (f) join LinearIso (x)
        assert!(analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_lineariso_used_twice_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let expr = Expr::Block {
            exprs: vec![call1("f", lvar("x")), call1("g", lvar("x"))],
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &expr);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains("x"), "error should name the binding: {}", msg);
                assert!(
                    msg.contains("linear"),
                    "error should mention linearity: {}",
                    msg
                );
            }
            other => panic!("expected CapError, got {:?}", other),
        }
        assert!(!analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_lineariso_never_used_errors() {
        // A LinearIso binding already present in the *initial* context
        // (e.g. a function parameter) must now be must-use checked.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let expr = Expr::Literal(Literal::Int(1), s());
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_let_bound_fresh_never_used_errors() {
        // let x: LinearIso = 1 :cap lineariso in 42 — `x` is a genuinely
        // fresh linear introduction (not a rebind) and is never referenced;
        // this must now be rejected (exactly-once/must-use).
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            fresh_lineariso(1),
            Expr::Literal(Literal::Int(42), s()),
        );
        let result = analyzer.infer_cap(&ctx, &expr);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains('x'), "error should name the binding: {}", msg);
                assert!(
                    msg.contains("never used"),
                    "error should describe a must-use violation: {}",
                    msg
                );
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    #[test]
    fn test_lineariso_let_bound_fresh_used_ok() {
        // let x: LinearIso = ... in f(x) — x is consumed, satisfies must-use.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr("x", fresh_lineariso(1), call1("f", lvar("x")));
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_lineariso_let_bound_transparent_rebind_never_used_ok() {
        // let x: LinearIso = ... in { let a = x; 1 } — `a` is a bare
        // rebind of `x`; evaluating `x` to initialize `a` already
        // discharges x's own must-use obligation, and `a` itself (a
        // transparent alias, never separately referenced) is exempt from
        // carrying a second, independent obligation for the same value.
        // Mirrors conformance/behavior/cap_13_lineariso_branch_merge_one_side_ok.nula.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            fresh_lineariso(1),
            let_expr("a", lvar("x"), Expr::Literal(Literal::Int(1), s())),
        );
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_lineariso_let_bound_consumed_on_only_one_branch_errors() {
        // let x: LinearIso = ... in if true then f(x) else 42 — the else
        // path never consumes x, and there is no code after the if to
        // catch up (unlike cap_13's outer rebind), so this is a genuine
        // must-use violation on the else-taken path.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            fresh_lineariso(1),
            Expr::If {
                cond: Box::new(Expr::Literal(Literal::Bool(true), s())),
                then_branch: Box::new(call1("f", lvar("x"))),
                else_branch: Some(Box::new(Expr::Literal(Literal::Int(42), s()))),
                span: s(),
            },
        );
        let result = analyzer.infer_cap(&ctx, &expr);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains('x'), "error should name the binding: {}", msg);
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    #[test]
    fn test_lineariso_let_bound_consumed_on_both_branches_ok() {
        // let x: LinearIso = ... in if true then f(x) else g(x) — both
        // branches consume x, so the post-merge set contains it: must-use
        // is satisfied.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            fresh_lineariso(1),
            Expr::If {
                cond: Box::new(Expr::Literal(Literal::Bool(true), s())),
                then_branch: Box::new(call1("f", lvar("x"))),
                else_branch: Some(Box::new(call1("g", lvar("x")))),
                span: s(),
            },
        );
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_lineariso_let_bound_consumed_via_explicit_consume_ok() {
        // let x: LinearIso = ... in consume x — explicit consume discharges
        // must-use.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            fresh_lineariso(1),
            Expr::Consume {
                expr: Box::new(lvar("x")),
                span: s(),
            },
        );
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_lineariso_let_bound_non_linear_never_used_ok() {
        // Regression guard: a non-linear let binding never needs must-use.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            Expr::Literal(Literal::Int(1), s()),
            Expr::Literal(Literal::Int(42), s()),
        );
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_linear_let_bound_fresh_never_used_errors() {
        // Same must-use discipline for plain `Linear` (not just LinearIso).
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new();
        let expr = let_expr(
            "x",
            Expr::CapAnnotate {
                expr: Box::new(Expr::Literal(Literal::Int(1), s())),
                cap: Capability::Linear,
                span: s(),
            },
            Expr::Literal(Literal::Int(42), s()),
        );
        let result = analyzer.infer_cap(&ctx, &expr);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains('x'), "error should name the binding: {}", msg);
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    #[test]
    fn test_lineariso_consumed_on_both_branches_then_used_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        // if c then f(x) else g(x); h(x) — both branches consume x, so the
        // binding is consumed after the if and the later use must fail.
        let expr = Expr::Block {
            exprs: vec![
                Expr::If {
                    cond: Box::new(Expr::Literal(Literal::Bool(true), s())),
                    then_branch: Box::new(call1("f", lvar("x"))),
                    else_branch: Some(Box::new(call1("g", lvar("x")))),
                    span: s(),
                },
                call1("h", lvar("x")),
            ],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_consumed_on_one_branch_then_used_ok() {
        // Conservative merge: a binding is consumed after an if only if ALL
        // fall-through paths consume it. The else branch here does not, so
        // the later use is fine.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let expr = Expr::Block {
            exprs: vec![
                Expr::If {
                    cond: Box::new(Expr::Literal(Literal::Bool(true), s())),
                    then_branch: Box::new(call1("f", lvar("x"))),
                    else_branch: Some(Box::new(Expr::Literal(Literal::Int(0), s()))),
                    span: s(),
                },
                call1("h", lvar("x")),
            ],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_lineariso_sent_once_ok() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::LinearIso);
        let cap = analyzer.infer_cap(&ctx, &send_m(lvar("x"))).unwrap();
        assert_eq!(cap, Capability::Val);
    }

    #[test]
    fn test_lineariso_sent_twice_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::LinearIso);
        // Sending a linear value consumes it (the spec'd linear move), so the
        // second send of the same binding must fail.
        let expr = Expr::Block {
            exprs: vec![send_m(lvar("x")), send_m(lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_downgrade_to_ref_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let expr = Expr::CapAnnotate {
            expr: Box::new(lvar("x")),
            cap: Capability::Ref,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &expr);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains("downgrade"), "unexpected message: {}", msg);
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    #[test]
    fn test_lineariso_discharge_linear_consumes() {
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let promote = Expr::CapAnnotate {
            expr: Box::new(lvar("x")),
            cap: Capability::Iso,
            span: s(),
        };
        // Promoting lineariso to iso discharges the linear obligation.
        let mut analyzer = CapabilityAnalyzer::new();
        let cap = analyzer.infer_cap(&ctx, &promote).unwrap();
        assert_eq!(cap, Capability::Iso);
        // ...but it still consumes the binding: a later use must fail.
        let mut analyzer = CapabilityAnalyzer::new();
        let expr = Expr::Block {
            exprs: vec![promote, call1("f", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_captured_by_closure_consumes() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let lam = Expr::Lambda {
            params: vec![Param::new("y", None)],
            ret_type: None,
            body: Box::new(lvar("x")),
            effect: None,
            span: s(),
        };
        let expr = Expr::Block {
            exprs: vec![lam, call1("f", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_consumed_in_for_body_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);
        let expr = Expr::For {
            var: "i".to_string(),
            iterable: Box::new(Expr::Array(vec![Expr::Literal(Literal::Int(1), s())], s())),
            body: Box::new(call1("f", lvar("x"))),
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_lineariso_shadowed_by_let_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::LinearIso);

        // The inner `x` is `Val` and used twice, which is fine for the inner `x`.
        // But the outer `x` (which is LinearIso) is completely shadowed and
        // thus never used. The block closes without discharging it, so it
        // must produce an error.
        let expr = Expr::Let {
            name: "x".to_string(),
            ty: None,
            value: Box::new(Expr::Literal(Literal::Int(1), s())),
            body: Box::new(Expr::Block {
                exprs: vec![call1("f", lvar("x")), call1("g", lvar("x"))],
                span: s(),
            }),
            mutable: false,
            span: s(),
            let_in: false,
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_iso_used_twice_ok() {
        // Regression: non-linear capabilities are unaffected.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Iso);
        let expr = Expr::Block {
            exprs: vec![call1("f", lvar("x")), call1("g", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
    }

    #[test]
    fn test_iso_sent_twice_errors() {
        // Sending an iso value transfers ownership; a second send must error.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::Iso);
        // send a, m(x); send a, n(x) — second send uses moved iso
        let expr = Expr::Block {
            exprs: vec![send_m(lvar("x")), send_m(lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
        assert!(
            analyzer
                .diagnostics
                .iter()
                .any(|d| d.contains("used after being moved")),
            "expected 'used after being moved' diagnostic, got: {:?}",
            analyzer.diagnostics
        );
    }

    #[test]
    fn test_iso_sent_once_ok() {
        // Sending an iso value once is fine; no second use follows.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new()
            .with_binding("a", Capability::Iso)
            .with_binding("x", Capability::Iso);
        let expr = send_m(lvar("x"));
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
        assert!(analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_iso_captured_by_closure_then_used_errors() {
        // Capturing an iso value in a closure transfers ownership;
        // subsequent use must error.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Iso);
        // { || send a, m(x); f(x) } — closure captures x (consumed), then
        // f(x) tries to use it again.
        let lam = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(send_m(lvar("x"))),
            effect: None,
            span: s(),
        };
        let expr = Expr::Block {
            exprs: vec![lam, call1("f", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
        assert!(
            analyzer
                .diagnostics
                .iter()
                .any(|d| d.contains("used after being moved")),
            "expected 'used after being moved' diagnostic, got: {:?}",
            analyzer.diagnostics
        );
    }

    #[test]
    fn test_iso_app_twice_ok() {
        // App (function call) with iso is NOT a move — the binding
        // remains usable.  Only send/ask/closure-capture consume iso.
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Iso);
        let expr = Expr::Block {
            exprs: vec![call1("f", lvar("x")), call1("g", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_ok());
        assert!(analyzer.diagnostics.is_empty());
    }

    // -----------------------------------------------------------------------
    // Linear capability tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_linear_used_once_ok() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let cap = analyzer.infer_cap(&ctx, &call1("f", lvar("x"))).unwrap();
        assert_eq!(cap, Capability::Val); // Val (f) join Linear (x)
        assert!(analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_linear_used_twice_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let expr = Expr::Block {
            exprs: vec![call1("f", lvar("x")), call1("g", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_linear_never_used_errors() {
        // Same must-use discipline for plain `Linear` (not just LinearIso).
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let expr = Expr::Literal(Literal::Int(1), s());
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_linear_sent_once_ok() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let cap = analyzer.infer_cap(&ctx, &send_m(lvar("x"))).unwrap();
        assert_eq!(cap, Capability::Val);
        assert!(analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_linear_remote_send_accepts() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let send = Expr::Send {
            actor: Box::new(lvar("a")),
            behavior: "m".to_string(),
            args: vec![lvar("x")],
            remote: true,
            span: s(),
        };
        let cap = analyzer.infer_cap(&ctx, &send).unwrap();
        assert_eq!(cap, Capability::Val);
        assert!(analyzer.diagnostics.is_empty());
    }

    #[test]
    fn test_linear_downgrade_to_ref_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let annotate = Expr::CapAnnotate {
            expr: Box::new(lvar("x")),
            cap: Capability::Ref,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &annotate);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains("downgrade"), "unexpected message: {}", msg);
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    #[test]
    fn test_linear_discharge_to_val_ok() {
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let annotate = Expr::CapAnnotate {
            expr: Box::new(lvar("x")),
            cap: Capability::Val,
            span: s(),
        };
        let mut analyzer = CapabilityAnalyzer::new();
        let cap = analyzer.infer_cap(&ctx, &annotate).unwrap();
        assert_eq!(cap, Capability::Val);
        // Discharging to Val consumes the binding.
        let mut analyzer = CapabilityAnalyzer::new();
        let expr = Expr::Block {
            exprs: vec![annotate, call1("f", lvar("x"))],
            span: s(),
        };
        assert!(analyzer.infer_cap(&ctx, &expr).is_err());
    }

    #[test]
    fn test_linear_discharge_to_iso_errors() {
        let mut analyzer = CapabilityAnalyzer::new();
        let ctx = CapContext::new().with_binding("x", Capability::Linear);
        let annotate = Expr::CapAnnotate {
            expr: Box::new(lvar("x")),
            cap: Capability::Iso,
            span: s(),
        };
        let result = analyzer.infer_cap(&ctx, &annotate);
        match result {
            Err(NuError::CapError { msg, .. }) => {
                assert!(msg.contains("downgrade"), "unexpected message: {}", msg);
            }
            other => panic!("expected CapError, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Interprocedural effect rows (module function row map + module flattening)
    // -----------------------------------------------------------------------

    fn parse_module(source: &str) -> AstModule {
        let mut lexer = crate::lexer::Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = crate::parser::Parser::new(tokens);
        parser.parse_module().unwrap()
    }

    #[test]
    fn test_flatten_decls_recurses_into_modules() {
        let ast = parse_module("module M { module N { fn f() 1 } }");
        let flat = flatten_decls(&ast.decls);
        assert_eq!(flat.len(), 1);
        assert!(
            matches!(flat[0], Decl::Function { name, .. } if name == "f"),
            "nested module function should be flattened to top level"
        );
    }

    #[test]
    fn test_register_function_rows_infers_unannotated_callee() {
        // `pure` is unannotated and calls the annotated `do_io`: its inferred
        // row must pick up IO (SPEC2 §4.9).
        let ast = parse_module(
            "fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
             fn pure() -> Unit { do_io() }",
        );
        let flat = flatten_decls(&ast.decls);
        let mut checker = EffectChecker::new();
        checker.register_function_rows(&flat).unwrap();
        assert!(checker.fn_rows["pure"].contains(&Effect::IO));
        assert!(checker.fn_rows["do_io"].contains(&Effect::IO));
    }

    #[test]
    fn test_register_function_rows_fixpoint_multi_hop() {
        // Call chain declared callee-last: `top` -> `middle` -> `do_io`.
        // One pass is not enough; the fixpoint must iterate until IO
        // propagates all the way to `top`.
        let ast = parse_module(
            "fn top() -> Unit { middle() }\n\
             fn middle() -> Unit { do_io() }\n\
             fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }",
        );
        let flat = flatten_decls(&ast.decls);
        let mut checker = EffectChecker::new();
        checker.register_function_rows(&flat).unwrap();
        assert!(checker.fn_rows["middle"].contains(&Effect::IO));
        assert!(checker.fn_rows["top"].contains(&Effect::IO));
    }

    #[test]
    fn test_register_function_rows_recursive_cycle_saturates() {
        // `a` and the annotated `b` call each other: the cycle saturates at
        // the fixpoint instead of looping forever.
        let ast = parse_module(
            "fn a() -> Unit { b() }\n\
             fn b() -> Unit ! {IO} { a() }",
        );
        let flat = flatten_decls(&ast.decls);
        let mut checker = EffectChecker::new();
        checker.register_function_rows(&flat).unwrap();
        assert!(checker.fn_rows["a"].contains(&Effect::IO));
    }

    #[test]
    fn test_check_module_rejects_pure_fn_calling_io_fn() {
        // Finding: `pure` declared `! {}` but (transitively) performing IO
        // through `do_io` must be rejected statically, not at runtime.
        let ast = parse_module(
            "fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
             fn pure() -> Unit ! {} { do_io() }",
        );
        let mut checker = EffectChecker::new();
        let result = checker.check_module(&ast.decls);
        assert!(
            result.is_err(),
            "pure function calling an IO function must be rejected"
        );
    }

    #[test]
    fn test_check_module_rejects_module_nested_effect_violation() {
        // Finding: declarations nested in `module {}` must be effect-checked
        // just like top-level ones.
        let ast =
            parse_module("module M {\n  fn pure() -> Unit ! {} { perform IO.print(\"x\") }\n}");
        let mut checker = EffectChecker::new();
        let result = checker.check_module(&ast.decls);
        assert!(
            result.is_err(),
            "module-nested pure function performing IO must be rejected"
        );
    }

    #[test]
    fn test_check_module_accepts_pure_functions() {
        // Positive: legitimately pure functions (including pure calls between
        // them) must keep passing.
        let ast = parse_module(
            "fn pure() -> Unit ! {} { unit }\n\
             fn also_pure() -> Unit ! {} { pure() }\n\
             module M { fn nested_pure() -> Unit ! {} { also_pure() } }",
        );
        let mut checker = EffectChecker::new();
        let result = checker.check_module(&ast.decls);
        assert!(
            result.is_ok(),
            "pure functions must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_check_module_accepts_matching_declared_effects() {
        // Positive: a function performing exactly its declared effects, plus
        // a caller whose row covers the callee's.
        let ast = parse_module(
            "fn do_io() -> Unit ! {IO} { perform IO.print(\"x\") }\n\
             fn caller() -> Unit ! {IO} { do_io() }",
        );
        let mut checker = EffectChecker::new();
        let result = checker.check_module(&ast.decls);
        assert!(
            result.is_ok(),
            "functions staying within declared rows must pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_resource_gate_blocks_ungranted_categories() {
        let ast = parse_module(
            "fn read_cfg() -> Unit ! {FS} { unit }\n\
             fn fetch_url() -> Unit ! {Net} { unit }\n\
             fn spawn_proc() -> Unit ! {System, Process} { unit }",
        );

        // No gate: standalone programs run with full access.
        let mut open = EffectChecker::new();
        assert!(
            open.check_module(&ast.decls).is_ok(),
            "ungated module must pass"
        );

        // Empty grant list: every resource category is denied.
        let mut denied = EffectChecker::new();
        denied.set_resource_grants(&[]);
        assert!(
            denied.check_module(&ast.decls).is_err(),
            "FS/Net/System must be denied without grants"
        );

        // Grant fs only: Net and System/Process remain denied.
        let mut fs_only = EffectChecker::new();
        fs_only.set_resource_grants(&["fs".to_string()]);
        assert!(
            fs_only.check_module(&ast.decls).is_err(),
            "Net/System must still be denied when only fs is granted"
        );

        // Grant everything: passes.
        let mut full = EffectChecker::new();
        full.set_resource_grants(&["fs".to_string(), "net".to_string(), "os".to_string()]);
        assert!(
            full.check_module(&ast.decls).is_ok(),
            "all resource categories granted must pass"
        );
    }

    #[test]
    fn test_shadowed_function_name_not_charged_callee_row() {
        // A local binding shadows the same-named module function: calling the
        // local (pure) closure must not be charged the module function's row.
        let mut checker = EffectChecker::new();
        checker
            .fn_rows
            .insert("do_io".to_string(), EffectRow::singleton(Effect::IO));
        let ctx = EffectContext::empty();
        let pure_lambda = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(Expr::Literal(Literal::Unit, s())),
            effect: None,
            span: s(),
        };
        let call = |name: &str| Expr::App {
            func: Box::new(Expr::Var(name.to_string(), s())),
            args: vec![],
            span: s(),
        };
        // let do_io = (|| unit) in do_io()  — shadowed: pure.
        let shadowed = Expr::Let {
            name: "do_io".to_string(),
            ty: None,
            value: Box::new(pure_lambda),
            body: Box::new(call("do_io")),
            mutable: false,
            span: s(),
            let_in: false,
        };
        assert!(
            checker
                .check_effects(&ctx, &shadowed, &EffectRow::empty())
                .is_ok(),
            "call through a shadowing local binding must be pure"
        );
        // Control: the unshadowed direct call must be charged IO.
        assert!(
            checker
                .check_effects(&ctx, &call("do_io"), &EffectRow::empty())
                .is_err(),
            "unshadowed direct call must propagate the callee row"
        );
    }

    #[test]
    fn test_state_machine_effect_checks_desugared_form() {
        // Hooks and the generated transition bodies are effect-checked
        // exactly like actor behaviors: the `perform IO.print` in the entry
        // hook infers an IO row, and un-annotated behaviors are
        // inference-only, so the module checks cleanly.
        let ast = parse_module(
            r#"
            state_machine M {
                state A
                state B
                event go: B
                on_entry B { perform IO.print("enter") }
                on_exit B { perform IO.print("leave") }
            }
            "#,
        );
        let mut checker = EffectChecker::new();
        let result = checker.check_module(&ast.decls);
        assert!(result.is_ok(), "expected success, got {:?}", result.err());
    }

    #[test]
    fn test_deprecation_warning_for_agent_declaration() {
        let ast = parse_module(
            r#"
            agent Assistant = {
                model: "gpt-4o",
                system_prompt: "You are helpful.",
                memory: { max_turns: 10 }
            }
            "#,
        );
        let mut checker = EffectChecker::new();
        assert!(checker.check_module(&ast.decls).is_ok());
        assert_eq!(checker.diagnostics.len(), 1);
        assert!(checker.diagnostics[0].contains("`agent` declaration 'Assistant' is deprecated"));
    }

    #[test]
    fn test_deprecation_warning_for_workflow_declaration() {
        let ast = parse_module(
            r#"
            workflow W {
                step a { 1 }
            }
            "#,
        );
        let mut checker = EffectChecker::new();
        assert!(checker.check_module(&ast.decls).is_ok());
        assert_eq!(checker.diagnostics.len(), 1);
        assert!(checker.diagnostics[0].contains("`workflow` declaration 'W' is deprecated"));
    }

    #[test]
    fn test_no_deprecation_warning_for_actor_declaration() {
        let ast = parse_module(
            r#"
            actor Counter {
                state count = 0
                behavior get() { self.count }
            }
            "#,
        );
        let mut checker = EffectChecker::new();
        assert!(checker.check_module(&ast.decls).is_ok());
        assert!(checker.diagnostics.is_empty());
    }
}
