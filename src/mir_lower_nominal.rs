//! Public MIR lowering facade that rejects ambiguous local actor dispatch.
//!
//! The legacy MIR lowerer still accepts a lexical receiver-name hint and, for
//! compatibility, may choose the first behavior with a matching short suffix.
//! HIR nominal-identity preservation makes statically proven actor references
//! exact. This facade closes the remaining local dynamic hole by validating
//! every send/ask before legacy lowering: known actor aliases must resolve
//! exactly, while opaque local refs may use a short behavior name only when it
//! is globally unique. Remote dispatch remains a runtime responsibility because
//! the target behavior may be supplied by a remotely loaded artifact.

#[path = "mir_lower.rs"]
mod legacy;

use crate::behavior_identity::resolve_behavior_index;
use crate::hir::{Body, Decl, Operand, RValue, Stmt};
use crate::types::{NuError, NuResult, Span};
use std::collections::HashSet;

pub fn lower_module(hir: &crate::hir::Module) -> NuResult<crate::mir::Module> {
    validate_local_dispatch(hir)?;
    legacy::lower_module(hir)
}

fn validate_local_dispatch(module: &crate::hir::Module) -> NuResult<()> {
    let mut actor_names = HashSet::new();
    let mut behavior_names = Vec::new();
    collect_actor_metadata(&module.decls, &mut actor_names, &mut behavior_names);
    validate_decls(&module.decls, &actor_names, &behavior_names)
}

fn collect_actor_metadata(
    decls: &[Decl],
    actor_names: &mut HashSet<String>,
    behavior_names: &mut Vec<String>,
) {
    for decl in decls {
        match decl {
            Decl::Actor(actor) => {
                actor_names.insert(actor.name.clone());
                behavior_names.extend(
                    actor
                        .behaviors
                        .iter()
                        .map(|behavior| format!("{}.{}", actor.name, behavior.name)),
                );
            }
            Decl::Module { decls, .. } => {
                collect_actor_metadata(decls, actor_names, behavior_names);
            }
            _ => {}
        }
    }
}

fn validate_decls(
    decls: &[Decl],
    actor_names: &HashSet<String>,
    behavior_names: &[String],
) -> NuResult<()> {
    for decl in decls {
        match decl {
            Decl::Function(function) => {
                validate_body(&function.body, actor_names, behavior_names)?;
            }
            Decl::Actor(actor) => {
                for behavior in &actor.behaviors {
                    validate_body(&behavior.body, actor_names, behavior_names)?;
                    if let Some(compensate) = &behavior.compensate {
                        validate_body(compensate, actor_names, behavior_names)?;
                    }
                }
            }
            Decl::Module { decls, .. } => {
                validate_decls(decls, actor_names, behavior_names)?;
            }
            Decl::Constant { body, .. } => {
                validate_body(body, actor_names, behavior_names)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_body(
    body: &Body,
    actor_names: &HashSet<String>,
    behavior_names: &[String],
) -> NuResult<()> {
    for stmt in &body.stmts {
        match stmt {
            Stmt::Let { value, span, .. } | Stmt::Assign { value, span, .. } => {
                validate_rvalue(value, *span, actor_names, behavior_names)?;
            }
            Stmt::StateSet { .. } | Stmt::Emit { .. } => {}
        }
    }
    Ok(())
}

fn validate_rvalue(
    value: &RValue,
    span: Span,
    actor_names: &HashSet<String>,
    behavior_names: &[String],
) -> NuResult<()> {
    match value {
        RValue::Closure { body, .. } | RValue::RecClosure { body, .. } => {
            validate_body(body, actor_names, behavior_names)?;
        }
        RValue::If {
            then_body,
            else_body,
            ..
        } => {
            validate_body(then_body, actor_names, behavior_names)?;
            if let Some(else_body) = else_body {
                validate_body(else_body, actor_names, behavior_names)?;
            }
        }
        RValue::Match { arms, .. } => {
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    validate_body(guard, actor_names, behavior_names)?;
                }
                validate_body(body, actor_names, behavior_names)?;
            }
        }
        RValue::For { body, .. } => validate_body(body, actor_names, behavior_names)?,
        RValue::While { cond, body, .. } => {
            validate_body(cond, actor_names, behavior_names)?;
            validate_body(body, actor_names, behavior_names)?;
        }
        RValue::Block(body) => validate_body(body, actor_names, behavior_names)?,
        RValue::Handle { body, handlers, .. } => {
            validate_body(body, actor_names, behavior_names)?;
            for handler in handlers {
                validate_body(&handler.body, actor_names, behavior_names)?;
            }
        }
        RValue::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    validate_body(guard, actor_names, behavior_names)?;
                }
                validate_body(body, actor_names, behavior_names)?;
            }
            if let Some((timeout, body)) = after {
                validate_body(timeout, actor_names, behavior_names)?;
                validate_body(body, actor_names, behavior_names)?;
            }
        }
        RValue::Send {
            actor,
            behavior,
            remote,
            ..
        }
        | RValue::Ask {
            actor,
            behavior,
            remote,
            ..
        } if !*remote => {
            let actor_hint = match actor {
                Operand::Var(name, _) => Some(name.as_str()),
                Operand::Literal(..) | Operand::Unit => None,
            };
            let proven_actor = actor_hint.filter(|name| actor_names.contains(*name));
            resolve_behavior_index(proven_actor, behavior, behavior_names).map_err(|error| {
                NuError::VMError {
                    msg: format!("invalid actor dispatch: {error}"),
                    span,
                }
            })?;
        }
        _ => {}
    }
    Ok(())
}