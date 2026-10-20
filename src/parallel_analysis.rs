//! Static safety analysis for source-level structured parallelism.
//!
//! `par { ... }` is still executed sequentially today, but RFC 0024 reserves
//! it for scoped concurrency. This module enforces invariants that must already
//! hold before a future backend is allowed to run branches concurrently.

use crate::ast::{Expr, Pattern};
use crate::effect_semantics::{classify_effect_operation, ParallelEffectConstraint};
use crate::types::{NuError, NuResult, Span};
use std::collections::{BTreeSet, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParallelBranchSummary {
    pub index: u32,
    /// Reads of names from outside this branch's lexical bindings.
    pub reads: BTreeSet<String>,
    /// Assignments/moves of names from outside this branch's lexical bindings.
    pub writes: BTreeSet<String>,
    /// Mutation through an outer aggregate/reference root (record/array/etc.).
    pub heap_mutations: BTreeSet<String>,
    /// Actor-state fields read by the branch.
    pub state_reads: BTreeSet<String>,
    /// Actor-state fields written by the branch.
    pub state_writes: BTreeSet<String>,
    /// Requested effect operations, for diagnostics and manifest handoff.
    pub effects: BTreeSet<String>,
    /// Strongest compiler-owned scheduling constraint imposed by operations in
    /// this branch. Capture/ownership analysis may impose stricter limits.
    pub effect_constraint: ParallelEffectConstraint,
    /// Whether an operation in this branch may suspend the computation.
    pub may_suspend: bool,
    /// Explicit external authority grants introduced by spawn sites.
    pub authorities: BTreeSet<String>,
    /// Control-flow escapes that cannot be scoped to one concurrent child yet.
    pub control_escapes: BTreeSet<String>,
    /// Mutable lambda formals without proven unique argument ownership.
    /// These are rejected until the scoped executor has alias provenance.
    pub unproven_parameter_aliases: BTreeSet<String>,
    /// Deferred cleanup currently depends on sequential scope-exit ordering.
    pub uses_defer: bool,
}

pub fn validate_parallel_branches(
    exprs: &[Expr],
    span: Span,
) -> NuResult<Vec<ParallelBranchSummary>> {
    let summaries: Vec<_> = exprs
        .iter()
        .enumerate()
        .map(|(index, expr)| summarize_branch(index as u32, expr))
        .collect();

    for branch in &summaries {
        if !branch.control_escapes.is_empty() {
            return Err(par_error(
                format!(
                    "par branch {} contains control-flow escape(s): {}; return/break/resume cannot escape a scoped concurrent branch",
                    branch.index,
                    branch
                        .control_escapes
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                span,
            ));
        }
        if !branch.unproven_parameter_aliases.is_empty() {
            return Err(par_error(
                format!(
                    "par branch {} mutates immediate lambda parameter(s) without an ownership proof ({}); aliasing of task arguments is not established",
                    branch.index,
                    branch
                        .unproven_parameter_aliases
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                span,
            ));
        }
        if branch.uses_defer {
            return Err(par_error(
                format!(
                    "par branch {} uses defer/errdefer; concurrent cleanup ordering is not defined yet",
                    branch.index
                ),
                span,
            ));
        }
        if !branch.state_writes.is_empty() {
            return Err(par_error(
                format!(
                    "par branch {} mutates actor state ({}); scoped tasks may read actor state but cannot mutate it until an explicit merge/transaction model is defined",
                    branch.index,
                    branch
                        .state_writes
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                span,
            ));
        }
    }

    for i in 0..summaries.len() {
        for j in (i + 1)..summaries.len() {
            if let Some((kind, name)) = branch_conflict(&summaries[i], &summaries[j]) {
                return Err(par_error(
                    format!(
                        "par branches {} and {} are not independent: {} '{}'",
                        summaries[i].index, summaries[j].index, kind, name
                    ),
                    span,
                ));
            }
        }
    }

    Ok(summaries)
}

pub fn summarize_branch(index: u32, expr: &Expr) -> ParallelBranchSummary {
    let mut summary = ParallelBranchSummary {
        index,
        ..ParallelBranchSummary::default()
    };
    summarize_expr(expr, &HashSet::new(), &mut summary);
    summary
}

fn constrain(out: &mut ParallelBranchSummary, constraint: ParallelEffectConstraint) {
    out.effect_constraint = out.effect_constraint.combine(constraint);
}

fn apply_effect_semantics(out: &mut ParallelBranchSummary, effect: &str, operation: &str) {
    let semantics = classify_effect_operation(effect, operation);
    constrain(out, semantics.parallel);
    out.may_suspend |= semantics.may_suspend;
}

fn par_error(msg: String, span: Span) -> NuError {
    NuError::TypeError {
        msg,
        span,
        expected_type: Some("independent scoped-concurrency branches".to_string()),
        found_type: Some("cross-branch mutation/dependency".to_string()),
        similar_names: None,
    }
}

fn branch_conflict(
    a: &ParallelBranchSummary,
    b: &ParallelBranchSummary,
) -> Option<(&'static str, String)> {
    for name in &a.writes {
        if b.reads.contains(name) || b.writes.contains(name) || b.heap_mutations.contains(name) {
            return Some(("outer binding write conflicts with access to", name.clone()));
        }
    }
    for name in &b.writes {
        if a.reads.contains(name) || a.writes.contains(name) || a.heap_mutations.contains(name) {
            return Some(("outer binding write conflicts with access to", name.clone()));
        }
    }
    for name in &a.heap_mutations {
        if b.reads.contains(name) || b.writes.contains(name) || b.heap_mutations.contains(name) {
            return Some((
                "shared aggregate mutation conflicts with access to",
                name.clone(),
            ));
        }
    }
    for name in &b.heap_mutations {
        if a.reads.contains(name) || a.writes.contains(name) || a.heap_mutations.contains(name) {
            return Some((
                "shared aggregate mutation conflicts with access to",
                name.clone(),
            ));
        }
    }

    // Reads of actor state are safe to duplicate, but any write would have
    // been rejected above. Keep the field sets in the summary so a future
    // transactional/merge model can selectively relax that rule.
    None
}

/// Resolve directly executed lambda expressions through transparent type and
/// capability annotations. Deferred/indirect closure calls are deliberately
/// outside this local proof; they require effect and alias analysis.
fn immediately_invoked_lambda(expr: &Expr) -> Option<(&[crate::ast::Param], &Expr)> {
    match expr {
        Expr::Lambda { params, body, .. } => Some((params, body)),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            immediately_invoked_lambda(expr)
        }
        _ => None,
    }
}

/// Only a direct variable reference (optionally annotated) establishes a
/// syntactically known alias. Arbitrary expressions need ownership provenance.
fn direct_alias_source(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Var(name, _) => Some(name.as_str()),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            direct_alias_source(expr)
        }
        _ => None,
    }
}

/// Conservatively detect aggregate writes through let-bound reference aliases.
/// The alias set is lexical: shadowing replaces its entry, and only a direct
/// variable binding propagates it. This is not a proof for indirect calls or
/// aliases hidden behind results returned by other functions.
fn mutates_aggregate_through_alias(expr: &Expr, aliases: &HashSet<String>) -> bool {
    match expr {
        Expr::Assign { target, value, .. } => {
            let writes_alias = matches!(
                target.as_ref(),
                Expr::FieldAccess { .. } | Expr::Index { .. }
            ) && root_var(target).is_some_and(|root| aliases.contains(root));
            writes_alias || mutates_aggregate_through_alias(value, aliases)
        }
        Expr::Let {
            name, value, body, ..
        } => {
            let in_initializer = mutates_aggregate_through_alias(value, aliases);
            let mut nested = aliases.clone();
            nested.remove(name);
            if direct_alias_source(value).is_some_and(|source| aliases.contains(source)) {
                nested.insert(name.clone());
            }
            in_initializer || mutates_aggregate_through_alias(body, &nested)
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => exprs
            .iter()
            .any(|part| mutates_aggregate_through_alias(part, aliases)),
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            mutates_aggregate_through_alias(cond, aliases)
                || mutates_aggregate_through_alias(then_branch, aliases)
                || else_branch.as_ref().is_some_and(|other| {
                    mutates_aggregate_through_alias(other, aliases)
                })
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            mutates_aggregate_through_alias(scrutinee, aliases)
                || arms.iter().any(|(pattern, guard, body)| {
                    let mut shadowed = HashSet::new();
                    pattern_bindings(pattern, &mut shadowed);
                    let nested: HashSet<String> =
                        aliases.difference(&shadowed).cloned().collect();
                    guard.as_ref().is_some_and(|check| {
                        mutates_aggregate_through_alias(check, &nested)
                    }) || mutates_aggregate_through_alias(body, &nested)
                })
        }
        Expr::While { cond, body, .. } => {
            mutates_aggregate_through_alias(cond, aliases)
                || mutates_aggregate_through_alias(body, aliases)
        }
        Expr::For {
            var, iterable, body, ..
        } => {
            let mut nested = aliases.clone();
            nested.remove(var);
            mutates_aggregate_through_alias(iterable, aliases)
                || mutates_aggregate_through_alias(body, &nested)
        }
        Expr::App { func, args, .. } => {
            let in_args = args
                .iter()
                .any(|arg| mutates_aggregate_through_alias(arg, aliases));
            if let Some((params, body)) = immediately_invoked_lambda(func) {
                let mut nested = aliases.clone();
                for param in params {
                    nested.remove(&param.name);
                }
                in_args || mutates_aggregate_through_alias(body, &nested)
            } else {
                in_args || mutates_aggregate_through_alias(func, aliases)
            }
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            mutates_aggregate_through_alias(left, aliases)
                || mutates_aggregate_through_alias(right, aliases)
        }
        Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Recover { body: expr, .. }
        | Expr::Hide { body: expr, .. }
        | Expr::Seal { body: expr, .. } => mutates_aggregate_through_alias(expr, aliases),
        // A closure declaration does not execute its body.
        Expr::Lambda { .. } => false,
        _ => false,
    }
}

fn summarize_expr(expr: &Expr, bound: &HashSet<String>, out: &mut ParallelBranchSummary) {
    match expr {
        Expr::Literal(..) | Expr::SelfRef(_) | Expr::Panic(..) => {}
        Expr::FString(parts, _) | Expr::Tuple(parts, _) | Expr::Array(parts, _) => {
            for part in parts {
                summarize_expr(part, bound, out);
            }
        }
        Expr::Var(name, _) => {
            if !bound.contains(name) {
                out.reads.insert(name.clone());
            }
        }
        Expr::Lambda { .. } => {
            // Creating a closure does not execute its body. Reference-capability
            // checking governs whether captured values may later cross a task
            // boundary.
            out.effects.insert("Closure.capture".to_string());
            constrain(out, ParallelEffectConstraint::ActorThreadOnly);
        }
        Expr::App { func, args, .. } => {
            summarize_expr(func, bound, out);
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            // Constructing a closure is inert, but immediately invoking one
            // executes its body in this branch. Account for captured writes,
            // actor-state mutations, and control effects instead of treating
            // the whole call as an opaque closure construction.
            //
            // Without alias provenance, mutation of an aggregate rooted at
            // a lambda parameter must fail closed: that formal can alias an
            // argument simultaneously read by a sibling branch. Summarize
            // once with the formal names considered free to reveal mutated
            // roots, then normally with them bound to find captured effects.
            // Indirect calls and rebinding aliases still require a separate
            // compiler ownership/effect proof before `par` runs concurrently.
            if let Some((params, body)) = immediately_invoked_lambda(func) {
                if !params.is_empty() {
                    let mut alias_scope = bound.clone();
                    for param in params {
                        alias_scope.remove(&param.name);
                    }
                    let mut alias_check = ParallelBranchSummary::default();
                    summarize_expr(body, &alias_scope, &mut alias_check);
                    for param in params {
                        if alias_check.heap_mutations.contains(&param.name) {
                            out.unproven_parameter_aliases.insert(param.name.clone());
                        }
                    }
                }
                let mut lambda_bound = bound.clone();
                lambda_bound.extend(params.iter().map(|param| param.name.clone()));
                // A return from the invoked function finishes that function,
                // not the enclosing scoped-concurrency branch. Preserve an
                // earlier branch-level return, if one was already observed.
                let enclosing_return = out.control_escapes.contains("return");
                summarize_expr(body, &lambda_bound, out);
                if !enclosing_return {
                    out.control_escapes.remove("return");
                }
            }
            out.effects.insert("Call".to_string());
            // An unknown callee may hide observable effects. This constraint
            // remains even after inspecting the immediate lambda body.
            constrain(out, ParallelEffectConstraint::SequentialOnly);
        }
        Expr::Let {
            name, value, body, ..
        } => {
            summarize_expr(value, bound, out);
            // A lexical alias can hide a write that the ordinary bound-name
            // summary ignores. Recover the original root when the initializer
            // is a direct variable reference. Local values with no alias
            // source (e.g. a fresh array literal) retain their existing rules.
            if let Some(source) = direct_alias_source(value) {
                let mut aliases = HashSet::new();
                aliases.insert(name.clone());
                if mutates_aggregate_through_alias(body, &aliases) {
                    if bound.contains(source) {
                        out.unproven_parameter_aliases.insert(source.to_string());
                    } else {
                        out.heap_mutations.insert(source.to_string());
                    }
                }
            }
            let mut nested = bound.clone();
            nested.insert(name.clone());
            summarize_expr(body, &nested, out);
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            let mut nested = bound.clone();
            nested.insert(name.clone());
            for param in params {
                nested.insert(param.name.clone());
            }
            // Conservative: recursive body construction may capture outer
            // values, so account for accesses even though execution is later.
            summarize_expr(value, &nested, out);
            let mut body_bound = bound.clone();
            body_bound.insert(name.clone());
            summarize_expr(body, &body_bound, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            summarize_expr(cond, bound, out);
            summarize_expr(then_branch, bound, out);
            if let Some(other) = else_branch {
                summarize_expr(other, bound, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            summarize_expr(scrutinee, bound, out);
            for (pattern, guard, body) in arms {
                let mut arm_bound = bound.clone();
                pattern_bindings(pattern, &mut arm_bound);
                if let Some(guard) = guard {
                    summarize_expr(guard, &arm_bound, out);
                }
                summarize_expr(body, &arm_bound, out);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for expr in exprs {
                summarize_expr(expr, bound, out);
            }
        }
        Expr::Record(fields, _) => {
            for (_, value) in fields {
                summarize_expr(value, bound, out);
            }
        }
        Expr::FieldAccess {
            expr: base, field, ..
        } => {
            if rooted_at_self(base) {
                out.state_reads.insert(state_path(base, field));
                // Even though the field is actor state, computed indices on
                // the path may themselves have effects or outer captures.
                summarize_state_indices(base, bound, out);
            } else {
                summarize_expr(base, bound, out);
            }
        }
        Expr::RecordUpdate { base, fields, .. } => {
            summarize_expr(base, bound, out);
            for (_, value) in fields {
                summarize_expr(value, bound, out);
            }
        }
        Expr::Index { arr, idx, .. } => {
            summarize_expr(arr, bound, out);
            summarize_expr(idx, bound, out);
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            summarize_expr(left, bound, out);
            summarize_expr(right, bound, out);
        }
        Expr::Unary { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. }
        | Expr::Recover { body: expr, .. }
        | Expr::Hide { body: expr, .. }
        | Expr::Seal { body: expr, .. } => summarize_expr(expr, bound, out),
        Expr::Assign { target, value, .. } => {
            summarize_assignment_target(target, bound, out);
            summarize_expr(value, bound, out);
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            capabilities,
            ..
        } => {
            summarize_expr(actor_type, bound, out);
            for (_, value) in init {
                summarize_expr(value, bound, out);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    summarize_expr(arg, bound, out);
                }
            }
            if let Some(node) = target_node {
                summarize_expr(node, bound, out);
            }
            out.effects.insert("Actor.spawn".to_string());
            apply_effect_semantics(out, "Actor", "spawn");
            out.authorities.extend(capabilities.iter().cloned());
        }
        Expr::Send {
            actor,
            behavior,
            args,
            ..
        } => {
            summarize_expr(actor, bound, out);
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert(format!("Actor.send.{behavior}"));
            apply_effect_semantics(out, "Actor", "send");
        }
        Expr::Ask {
            actor,
            behavior,
            args,
            ..
        } => {
            summarize_expr(actor, bound, out);
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert(format!("Actor.ask.{behavior}"));
            apply_effect_semantics(out, "Actor", "ask");
        }
        Expr::Receive { arms, after, .. } => {
            out.effects.insert("Actor.receive".to_string());
            apply_effect_semantics(out, "Actor", "receive");
            for (_, patterns, guard, body) in arms {
                let mut arm_bound = bound.clone();
                for pattern in patterns {
                    pattern_bindings(pattern, &mut arm_bound);
                }
                if let Some(guard) = guard {
                    summarize_expr(guard, &arm_bound, out);
                }
                summarize_expr(body, &arm_bound, out);
            }
            if let Some((timeout, body)) = after {
                summarize_expr(timeout, bound, out);
                summarize_expr(body, bound, out);
            }
        }
        Expr::Emit { event, args, .. } => {
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert(format!("Event.emit.{event}"));
            apply_effect_semantics(out, "Event", "emit");
        }
        Expr::Perform {
            effect, op, args, ..
        } => {
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert(format!("{effect}.{op}"));
            apply_effect_semantics(out, effect, op);
        }
        Expr::GrainRef { key, .. } => {
            summarize_expr(key, bound, out);
            out.effects.insert("Grain.ref".to_string());
            constrain(out, ParallelEffectConstraint::ActorThreadOnly);
        }
        Expr::Resume { value, .. } => {
            summarize_expr(value, bound, out);
            out.control_escapes.insert("resume".to_string());
        }
        Expr::Handle { body, handlers, .. } => {
            summarize_expr(body, bound, out);
            for handler in handlers {
                let mut handler_bound = bound.clone();
                handler_bound.extend(handler.params.iter().cloned());
                summarize_expr(&handler.body, &handler_bound, out);
            }
        }
        Expr::Migrate { actor, node, .. } => {
            summarize_expr(actor, bound, out);
            summarize_expr(node, bound, out);
            out.effects.insert("Actor.migrate".to_string());
            apply_effect_semantics(out, "Actor", "migrate");
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            summarize_expr(iterable, bound, out);
            let mut nested = bound.clone();
            nested.insert(var.clone());
            summarize_expr(body, &nested, out);
        }
        Expr::While { cond, body, .. } => {
            summarize_expr(cond, bound, out);
            summarize_expr(body, bound, out);
        }
        Expr::Return(value, _) => {
            if let Some(value) = value {
                summarize_expr(value, bound, out);
            }
            out.control_escapes.insert("return".to_string());
        }
        Expr::Break(value, _) => {
            if let Some(value) = value {
                summarize_expr(value, bound, out);
            }
            out.control_escapes.insert("break".to_string());
        }
        Expr::Consume { expr, .. } => {
            summarize_expr(expr, bound, out);
            if rooted_at_self(expr) {
                // Moving out of actor-owned state is a mutation, even when
                // there is no ordinary root variable to record as a write.
                out.state_writes.insert(state_target_path(expr));
            } else if let Some(name) = root_var(expr) {
                if !bound.contains(name) {
                    out.writes.insert(name.to_string());
                }
            }
        }
        Expr::Defer { expr, .. } => {
            summarize_expr(expr, bound, out);
            out.uses_defer = true;
        }
    }
}

fn summarize_assignment_target(
    target: &Expr,
    bound: &HashSet<String>,
    out: &mut ParallelBranchSummary,
) {
    match target {
        Expr::SelfRef(_) => {
            out.state_writes.insert("self".to_string());
        }
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            summarize_assignment_target(expr, bound, out);
        }
        Expr::Var(name, _) => {
            if !bound.contains(name) {
                out.writes.insert(name.clone());
            }
        }
        Expr::FieldAccess {
            expr: base, field, ..
        } if rooted_at_self(base) => {
            out.state_writes.insert(state_path(base, field));
            summarize_state_indices(base, bound, out);
        }
        Expr::FieldAccess { expr: base, .. } => {
            summarize_expr(base, bound, out);
            if let Some(name) = root_var(base) {
                if !bound.contains(name) {
                    out.heap_mutations.insert(name.to_string());
                }
            }
        }
        Expr::Index { arr, idx, .. } if rooted_at_self(arr) => {
            // Index assignment mutates actor state even when the target is
            // self.items[i] rather than a direct self.field assignment.
            out.state_writes.insert(state_target_path(target));
            summarize_state_indices(arr, bound, out);
            summarize_expr(idx, bound, out);
        }
        Expr::Index { arr, idx, .. } => {
            summarize_expr(arr, bound, out);
            summarize_expr(idx, bound, out);
            if let Some(name) = root_var(arr) {
                if !bound.contains(name) {
                    out.heap_mutations.insert(name.to_string());
                }
            }
        }
        other => {
            summarize_expr(other, bound, out);
            if let Some(name) = root_var(other) {
                if !bound.contains(name) {
                    out.heap_mutations.insert(name.to_string());
                }
            }
        }
    }
}

/// Collect effects and captures from computed indices on a self-rooted path.
/// FieldAccess on self is summarized as one state read, so normal recursion
/// would otherwise skip indices nested under that field access.
fn summarize_state_indices(expr: &Expr, bound: &HashSet<String>, out: &mut ParallelBranchSummary) {
    match expr {
        Expr::Index { arr, idx, .. } => {
            summarize_state_indices(arr, bound, out);
            summarize_expr(idx, bound, out);
        }
        Expr::FieldAccess { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. } => summarize_state_indices(expr, bound, out),
        _ => {}
    }
}

fn state_target_path(expr: &Expr) -> String {
    match expr {
        Expr::SelfRef(_) => "self".to_string(),
        Expr::FieldAccess { expr, field, .. } => {
            format!("{}.{}", state_target_path(expr), field)
        }
        Expr::Index { arr, .. } => format!("{}[*]", state_target_path(arr)),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => state_target_path(expr),
        _ => "self".to_string(),
    }
}

fn root_var(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Var(name, _) => Some(name.as_str()),
        Expr::FieldAccess { expr, .. } => root_var(expr),
        Expr::Index { arr, .. } => root_var(arr),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => root_var(expr),
        _ => None,
    }
}

fn rooted_at_self(expr: &Expr) -> bool {
    match expr {
        Expr::SelfRef(_) => true,
        Expr::FieldAccess { expr, .. } => rooted_at_self(expr),
        Expr::Index { arr, .. } => rooted_at_self(arr),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => rooted_at_self(expr),
        _ => false,
    }
}

fn state_path(base: &Expr, leaf: &str) -> String {
    fn collect(expr: &Expr, parts: &mut Vec<String>) {
        match expr {
            Expr::SelfRef(_) => {}
            Expr::FieldAccess { expr, field, .. } => {
                collect(expr, parts);
                parts.push(field.clone());
            }
            _ => {}
        }
    }

    let mut parts = Vec::new();
    collect(base, &mut parts);
    parts.push(leaf.to_string());
    parts.join(".")
}

fn pattern_bindings(pattern: &Pattern, bound: &mut HashSet<String>) {
    match pattern {
        Pattern::Wild | Pattern::Lit(_) => {}
        Pattern::Var(name) => {
            bound.insert(name.clone());
        }
        Pattern::Alias(name, inner) => {
            bound.insert(name.clone());
            pattern_bindings(inner, bound);
        }
        Pattern::Tuple(items) => {
            for item in items {
                pattern_bindings(item, bound);
            }
        }
        Pattern::Record(fields) => {
            for (_, item) in fields {
                pattern_bindings(item, bound);
            }
        }
        Pattern::Variant(_, Some(inner)) => pattern_bindings(inner, bound),
        Pattern::Variant(_, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinOp, Literal};

    fn sp() -> Span {
        Span::default()
    }

    fn var(name: &str) -> Expr {
        Expr::Var(name.to_string(), sp())
    }

    fn int(n: i64) -> Expr {
        Expr::Literal(Literal::Int(n), sp())
    }

    #[test]
    fn independent_reads_are_allowed() {
        let branches = vec![
            Expr::Binary {
                op: BinOp::Add,
                left: Box::new(var("x")),
                right: Box::new(int(1)),
                span: sp(),
            },
            Expr::Binary {
                op: BinOp::Add,
                left: Box::new(var("y")),
                right: Box::new(int(1)),
                span: sp(),
            },
        ];
        assert!(validate_parallel_branches(&branches, sp()).is_ok());
    }

    #[test]
    fn cross_branch_write_read_dependency_is_rejected() {
        let branches = vec![
            Expr::Assign {
                target: Box::new(var("x")),
                value: Box::new(int(1)),
                span: sp(),
            },
            var("x"),
        ];
        assert!(validate_parallel_branches(&branches, sp()).is_err());
    }

    #[test]
    fn actor_state_mutation_is_rejected() {
        let branches = vec![
            Expr::Assign {
                target: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(sp())),
                    field: "count".to_string(),
                    span: sp(),
                }),
                value: Box::new(int(1)),
                span: sp(),
            },
            int(2),
        ];
        assert!(validate_parallel_branches(&branches, sp()).is_err());
    }

    #[test]
    fn indexed_actor_state_mutation_is_rejected() {
        let branches = vec![
            Expr::Assign {
                target: Box::new(Expr::Index {
                    arr: Box::new(Expr::FieldAccess {
                        expr: Box::new(Expr::SelfRef(sp())),
                        field: "items".to_string(),
                        span: sp(),
                    }),
                    idx: Box::new(int(0)),
                    span: sp(),
                }),
                value: Box::new(int(1)),
                span: sp(),
            },
            int(2),
        ];
        let err = validate_parallel_branches(&branches, sp()).unwrap_err();
        assert!(err.to_string().contains("mutates actor state"));
    }

    #[test]
    fn annotated_indexed_actor_state_mutation_is_rejected() {
        let target = Expr::CapAnnotate {
            expr: Box::new(Expr::Index {
                arr: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(sp())),
                    field: "items".to_string(),
                    span: sp(),
                }),
                idx: Box::new(int(0)),
                span: sp(),
            }),
            cap: crate::types::Capability::Iso,
            span: sp(),
        };
        let branches = vec![
            Expr::Assign {
                target: Box::new(target),
                value: Box::new(int(1)),
                span: sp(),
            },
            int(2),
        ];
        let err = validate_parallel_branches(&branches, sp()).unwrap_err();
        assert!(err.to_string().contains("mutates actor state"));
    }

    #[test]
    fn consuming_indexed_actor_state_is_rejected() {
        let branches = vec![
            Expr::Consume {
                expr: Box::new(Expr::Index {
                    arr: Box::new(Expr::FieldAccess {
                        expr: Box::new(Expr::SelfRef(sp())),
                        field: "items".to_string(),
                        span: sp(),
                    }),
                    idx: Box::new(int(0)),
                    span: sp(),
                }),
                span: sp(),
            },
            int(2),
        ];
        let err = validate_parallel_branches(&branches, sp()).unwrap_err();
        assert!(err.to_string().contains("mutates actor state"));
    }

    #[test]
    fn consuming_actor_state_is_rejected() {
        let branches = vec![
            Expr::Consume {
                expr: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(sp())),
                    field: "items".to_string(),
                    span: sp(),
                }),
                span: sp(),
            },
            int(2),
        ];
        let err = validate_parallel_branches(&branches, sp()).unwrap_err();
        assert!(err.to_string().contains("mutates actor state"));
    }

    #[test]
    fn nested_actor_state_index_effect_is_summarized() {
        let branch = Expr::FieldAccess {
            expr: Box::new(Expr::Index {
                arr: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(sp())),
                    field: "items".to_string(),
                    span: sp(),
                }),
                idx: Box::new(Expr::Perform {
                    effect: "IO".to_string(),
                    op: "read".to_string(),
                    args: vec![],
                    span: sp(),
                }),
                span: sp(),
            }),
            field: "value".to_string(),
            span: sp(),
        };
        let summary = summarize_branch(0, &branch);
        assert!(summary.effects.contains("IO.read"));
    }

    #[test]
    fn effects_and_authority_are_summarized() {
        let branch = Expr::Spawn {
            actor_type: Box::new(var("Worker")),
            init: vec![],
            positional_args: None,
            register_as: None,
            target_node: None,
            capabilities: vec!["Net::TcpOut(api.example.com:443)".to_string()],
            span: sp(),
        };
        let summary = summarize_branch(0, &branch);
        assert!(summary.effects.contains("Actor.spawn"));
        assert!(summary
            .authorities
            .contains("Net::TcpOut(api.example.com:443)"));
    }

    #[test]
    fn immediate_lambda_mutation_through_local_alias_is_rejected() {
        let mutating = Expr::Lambda {
            params: vec![crate::ast::Param::new("view", None)],
            ret_type: None,
            body: Box::new(Expr::Let {
                name: "alias".to_string(),
                ty: None,
                value: Box::new(var("view")),
                body: Box::new(Expr::Assign {
                    target: Box::new(Expr::Index {
                        arr: Box::new(var("alias")),
                        idx: Box::new(int(0)),
                        span: sp(),
                    }),
                    value: Box::new(int(5)),
                    span: sp(),
                }),
                mutable: false,
                let_in: true,
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        assert!(
            validate_parallel_branches(
                &[
                    Expr::App {
                        func: Box::new(mutating),
                        args: vec![var("shared")],
                        span: sp(),
                    },
                    var("shared"),
                ],
                sp()
            )
            .is_err(),
            "a let-bound alias must not conceal mutation of a lambda parameter"
        );
    }

    #[test]
    fn immediate_lambda_locally_allocated_alias_is_not_a_parameter_alias() {
        let mutating_local = Expr::Lambda {
            params: vec![crate::ast::Param::new("view", None)],
            ret_type: None,
            body: Box::new(Expr::Let {
                name: "alias".to_string(),
                ty: None,
                value: Box::new(Expr::Array(vec![int(1)], sp())),
                body: Box::new(Expr::Assign {
                    target: Box::new(Expr::Index {
                        arr: Box::new(var("alias")),
                        idx: Box::new(int(0)),
                        span: sp(),
                    }),
                    value: Box::new(int(5)),
                    span: sp(),
                }),
                mutable: false,
                let_in: true,
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        assert!(
            validate_parallel_branches(
                &[
                    Expr::App {
                        func: Box::new(mutating_local),
                        args: vec![var("shared")],
                        span: sp(),
                    },
                    var("shared"),
                ],
                sp()
            )
            .is_ok(),
            "a proven local allocation is not a direct alias of the parameter"
        );
    }

    #[test]
    fn immediately_invoked_lambda_cannot_mutate_unproven_reference_argument() {
        let mutating = Expr::Lambda {
            params: vec![crate::ast::Param::new("view", None)],
            ret_type: None,
            body: Box::new(Expr::Assign {
                target: Box::new(Expr::Index {
                    arr: Box::new(var("view")),
                    idx: Box::new(int(0)),
                    span: sp(),
                }),
                value: Box::new(int(5)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        let branches = vec![
            Expr::App {
                func: Box::new(mutating),
                args: vec![var("shared")],
                span: sp(),
            },
            var("shared"),
        ];
        assert!(
            validate_parallel_branches(&branches, sp()).is_err(),
            "a mutable formal parameter can alias an argument read by a sibling"
        );
    }

    #[test]
    fn immediately_invoked_lambda_checks_formal_even_if_outer_scope_shadows_name() {
        let mutating = Expr::Lambda {
            params: vec![crate::ast::Param::new("shared", None)],
            ret_type: None,
            body: Box::new(Expr::Assign {
                target: Box::new(Expr::FieldAccess {
                    expr: Box::new(var("shared")),
                    field: "count".to_string(),
                    span: sp(),
                }),
                value: Box::new(int(5)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        let branches = vec![Expr::Let {
            name: "shared".to_string(),
            ty: None,
            value: Box::new(var("source")),
            body: Box::new(Expr::App {
                func: Box::new(mutating),
                args: vec![var("shared")],
                span: sp(),
            }),
            mutable: false,
            let_in: true,
            span: sp(),
        }];
        assert!(
            validate_parallel_branches(&branches, sp()).is_err(),
            "the inner lambda parameter must not inherit the outer bound name"
        );
    }

    #[test]
    fn immediate_lambda_return_is_local_to_lambda_scope() {
        let returning = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(Expr::Return(Some(Box::new(int(7))), sp())),
            effect: None,
            span: sp(),
        };
        let branches = vec![
            Expr::App {
                func: Box::new(returning),
                args: vec![],
                span: sp(),
            },
            int(3),
        ];
        assert!(
            validate_parallel_branches(&branches, sp()).is_ok(),
            "a return from an invoked lambda must not escape the par branch"
        );
    }

    #[test]
    fn immediately_invoked_lambda_pure_parameter_read_is_allowed() {
        let pure = Expr::Lambda {
            params: vec![crate::ast::Param::new("value", None)],
            ret_type: None,
            body: Box::new(Expr::Binary {
                op: BinOp::Add,
                left: Box::new(var("value")),
                right: Box::new(int(1)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        let branches = vec![
            Expr::App {
                func: Box::new(pure),
                args: vec![var("shared")],
                span: sp(),
            },
            var("shared"),
        ];
        assert!(validate_parallel_branches(&branches, sp()).is_ok());
    }

    #[test]
    fn immediate_lambda_captured_heap_mutation_conflicts_with_other_branch_read() {
        let mutating = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(Expr::Assign {
                target: Box::new(Expr::Index {
                    arr: Box::new(var("shared")),
                    idx: Box::new(int(0)),
                    span: sp(),
                }),
                value: Box::new(int(5)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        let branches = vec![
            Expr::App {
                func: Box::new(mutating),
                args: vec![],
                span: sp(),
            },
            var("shared"),
        ];
        assert!(
            validate_parallel_branches(&branches, sp()).is_err(),
            "immediate lambda execution must not hide a captured heap mutation"
        );
    }

    #[test]
    fn immediate_lambda_actor_state_write_is_rejected() {
        let mutating = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(Expr::Assign {
                target: Box::new(Expr::FieldAccess {
                    expr: Box::new(Expr::SelfRef(sp())),
                    field: "counter".to_string(),
                    span: sp(),
                }),
                value: Box::new(int(5)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        assert!(
            validate_parallel_branches(
                &[Expr::App {
                    func: Box::new(mutating),
                    args: vec![],
                    span: sp(),
                }],
                sp()
            )
            .is_err(),
            "calling a lambda must not bypass actor-state isolation"
        );
    }

    #[test]
    fn constructing_a_lambda_does_not_execute_its_body() {
        let deferred = Expr::Lambda {
            params: vec![],
            ret_type: None,
            body: Box::new(Expr::Assign {
                target: Box::new(var("shared")),
                value: Box::new(int(7)),
                span: sp(),
            }),
            effect: None,
            span: sp(),
        };
        assert!(validate_parallel_branches(&[deferred, var("shared")], sp()).is_ok());
    }

    #[test]
    fn immediately_invoked_lambda_exposes_suspension_and_captured_mutation() {
        let branch = Expr::App {
            func: Box::new(Expr::Lambda {
                params: vec![],
                ret_type: None,
                body: Box::new(Expr::Block {
                    exprs: vec![
                        Expr::Perform {
                            effect: "Timer".to_string(),
                            op: "sleep".to_string(),
                            args: vec![int(10)],
                            span: sp(),
                        },
                        Expr::Assign {
                            target: Box::new(Expr::Index {
                                arr: Box::new(var("shared")),
                                idx: Box::new(int(0)),
                                span: sp(),
                            }),
                            value: Box::new(int(5)),
                            span: sp(),
                        },
                    ],
                    span: sp(),
                }),
                effect: None,
                span: sp(),
            }),
            args: vec![],
            span: sp(),
        };
        let summary = summarize_branch(0, &branch);
        assert!(summary.may_suspend, "lambda execution must propagate may_suspend");
        assert!(summary.effects.contains("Timer.sleep"));
        assert_eq!(
            summary.effect_constraint,
            ParallelEffectConstraint::SequentialOnly
        );
        assert!(
            validate_parallel_branches(&[branch, var("shared")], sp()).is_err(),
            "effect and capture checks must agree on the executed body"
        );
    }

    #[test]
    fn immediate_lambda_mutable_alias_still_requires_ownership_proof_with_effects() {
        let branch = Expr::App {
            func: Box::new(Expr::Lambda {
                params: vec![crate::ast::Param::new("view", None)],
                ret_type: None,
                body: Box::new(Expr::Assign {
                    target: Box::new(Expr::Index {
                        arr: Box::new(var("view")),
                        idx: Box::new(int(0)),
                        span: sp(),
                    }),
                    value: Box::new(int(5)),
                    span: sp(),
                }),
                effect: None,
                span: sp(),
            }),
            args: vec![var("shared")],
            span: sp(),
        };
        assert!(
            validate_parallel_branches(&[branch, var("shared")], sp()).is_err(),
            "the effect-analysis stack must retain the alias fail-closed rule"
        );
    }

}
