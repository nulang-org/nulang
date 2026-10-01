//! Static safety analysis for source-level structured parallelism.
//!
//! `par { ... }` is still executed sequentially today, but RFC 0024 reserves
//! it for scoped concurrency. This module enforces invariants that must already
//! hold before a future backend is allowed to run branches concurrently.

use crate::ast::{Expr, Pattern};
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
    /// Requested effect operations, for later effect-policy scheduling.
    pub effects: BTreeSet<String>,
    /// Explicit external authority grants introduced by spawn sites.
    pub authorities: BTreeSet<String>,
    /// Control-flow escapes that cannot be scoped to one concurrent child yet.
    pub control_escapes: BTreeSet<String>,
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
        }
        Expr::App { func, args, .. } => {
            summarize_expr(func, bound, out);
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert("Call".to_string());
        }
        Expr::Let {
            name, value, body, ..
        } => {
            summarize_expr(value, bound, out);
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
        }
        Expr::Receive { arms, after, .. } => {
            out.effects.insert("Actor.receive".to_string());
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
        }
        Expr::Perform {
            effect, op, args, ..
        } => {
            for arg in args {
                summarize_expr(arg, bound, out);
            }
            out.effects.insert(format!("{effect}.{op}"));
        }
        Expr::GrainRef { key, .. } => {
            summarize_expr(key, bound, out);
            out.effects.insert("Grain.ref".to_string());
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
            if let Some(name) = root_var(expr) {
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
        Expr::Var(name, _) => {
            if !bound.contains(name) {
                out.writes.insert(name.clone());
            }
        }
        Expr::FieldAccess {
            expr: base, field, ..
        } if rooted_at_self(base) => {
            out.state_writes.insert(state_path(base, field));
        }
        Expr::FieldAccess { expr: base, .. } => {
            summarize_expr(base, bound, out);
            if let Some(name) = root_var(base) {
                if !bound.contains(name) {
                    out.heap_mutations.insert(name.to_string());
                }
            }
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
}
