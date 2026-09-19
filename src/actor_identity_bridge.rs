//! Preserve statically known actor identity across HIR -> MIR lowering.
//!
//! The legacy MIR resolver already has a correct exact-match path when the
//! receiver operand's variable name is the actor schema (`Actor.behavior`).
//! What HIR normally carries, however, is the source binding name (`target`),
//! so duplicate short behavior names fall through to a global first-suffix
//! match.
//!
//! This pass does not change user-visible names or the MIR representation.
//! Instead, each statically known send/ask is wrapped in a scoped HIR block
//! that aliases the receiver under its proven actor schema name. MIR's normal
//! lexical scope handling makes that alias visible only for the dispatch and
//! removes it immediately afterwards.
//!
//! Dynamic/opaque actor refs are deliberately left untouched. Runtime actor
//! ownership remains the defense-in-depth boundary for those calls.

use crate::ast::Pattern;
use crate::hir::{self, Body, Decl, Operand, Place, RValue, Stmt, Terminator};
use crate::types::Span;
use rustc_hash::FxHashMap;
use std::collections::HashSet;

#[derive(Default)]
struct Bridge {
    next_temp: u64,
}

pub(super) fn preserve_nominal_dispatch_identity(module: &mut hir::Module) {
    let mut bridge = Bridge::default();
    bridge.transform_decls(&mut module.decls);
}

impl Bridge {
    fn transform_decls(&mut self, decls: &mut [Decl]) {
        for decl in decls {
            match decl {
                Decl::Function(function) => {
                    self.transform_body(&mut function.body, FxHashMap::default(), None);
                }
                Decl::Actor(actor) => {
                    for behavior in &mut actor.behaviors {
                        let mut env = FxHashMap::default();
                        env.insert("self".to_string(), actor.name.clone());
                        self.transform_body(
                            &mut behavior.body,
                            env.clone(),
                            Some(actor.name.as_str()),
                        );
                        if let Some(compensate) = &mut behavior.compensate {
                            self.transform_body(compensate, env.clone(), Some(actor.name.as_str()));
                        }
                    }
                }
                Decl::Module { decls, .. } => self.transform_decls(decls),
                Decl::Constant { body, .. } => {
                    self.transform_body(body, FxHashMap::default(), None);
                }
                _ => {}
            }
        }
    }

    fn transform_body(
        &mut self,
        body: &mut Body,
        mut env: FxHashMap<String, String>,
        current_actor: Option<&str>,
    ) {
        for stmt in &mut body.stmts {
            match stmt {
                Stmt::Let {
                    name, value, span, ..
                } => {
                    let invalidated = invalidated_actor_bindings(value, &env);
                    self.transform_rvalue(value, &env, current_actor, *span);
                    for binding in invalidated {
                        env.remove(&binding);
                    }

                    let identity = actor_identity_of_rvalue(value, &env, current_actor);
                    env.remove(name);
                    if let Some(identity) = identity {
                        env.insert(name.clone(), identity);
                    }
                }
                Stmt::Assign {
                    target,
                    value,
                    span,
                } => {
                    let invalidated = invalidated_actor_bindings(value, &env);
                    self.transform_rvalue(value, &env, current_actor, *span);
                    for binding in invalidated {
                        env.remove(&binding);
                    }

                    if let Place::Var(name, _) = target {
                        let identity = actor_identity_of_rvalue(value, &env, current_actor);
                        env.remove(name);
                        if let Some(identity) = identity {
                            env.insert(name.clone(), identity);
                        }
                    }
                }
                Stmt::StateSet { .. } | Stmt::Emit { .. } => {}
            }
        }
    }

    fn transform_rvalue(
        &mut self,
        value: &mut RValue,
        env: &FxHashMap<String, String>,
        current_actor: Option<&str>,
        span: Span,
    ) {
        match value {
            RValue::Closure { params, body, .. } => {
                let mut nested = env.clone();
                for (name, _) in params {
                    nested.remove(name);
                }
                self.transform_body(body, nested, current_actor);
            }
            RValue::RecClosure {
                name, params, body, ..
            } => {
                let mut nested = env.clone();
                nested.remove(name);
                for (name, _) in params {
                    nested.remove(name);
                }
                self.transform_body(body, nested, current_actor);
            }
            RValue::If {
                then_body,
                else_body,
                ..
            } => {
                self.transform_body(then_body, env.clone(), current_actor);
                if let Some(else_body) = else_body {
                    self.transform_body(else_body, env.clone(), current_actor);
                }
            }
            RValue::Match { arms, .. } => {
                for (pattern, guard, arm) in arms {
                    let mut nested = env.clone();
                    shadow_pattern(&mut nested, pattern);
                    if let Some(guard) = guard {
                        self.transform_body(guard, nested.clone(), current_actor);
                    }
                    self.transform_body(arm, nested, current_actor);
                }
            }
            RValue::For { var, body, .. } => {
                let mut nested = env.clone();
                nested.remove(var);
                self.transform_body(body, nested, current_actor);
            }
            RValue::While { cond, body, .. } => {
                self.transform_body(cond, env.clone(), current_actor);
                self.transform_body(body, env.clone(), current_actor);
            }
            RValue::Block(body) => {
                self.transform_body(body, env.clone(), current_actor);
            }
            RValue::Handle { body, handlers, .. } => {
                self.transform_body(body, env.clone(), current_actor);
                for handler in handlers {
                    let mut nested = env.clone();
                    for (name, _) in &handler.params {
                        nested.remove(name);
                    }
                    self.transform_body(&mut handler.body, nested, current_actor);
                }
            }
            RValue::Receive { arms, after, .. } => {
                for (_, patterns, guard, arm) in arms {
                    let mut nested = env.clone();
                    for pattern in patterns {
                        shadow_pattern(&mut nested, pattern);
                    }
                    if let Some(guard) = guard {
                        self.transform_body(guard, nested.clone(), current_actor);
                    }
                    self.transform_body(arm, nested, current_actor);
                }
                if let Some((timeout, after_body)) = after {
                    self.transform_body(timeout, env.clone(), current_actor);
                    self.transform_body(after_body, env.clone(), current_actor);
                }
            }
            RValue::Send { actor, .. } | RValue::Ask { actor, .. } => {
                if let Some(actor_schema) = actor_identity_of_operand(actor, env) {
                    self.wrap_dispatch(value, &actor_schema, span);
                }
            }
            _ => {}
        }
    }

    fn wrap_dispatch(&mut self, value: &mut RValue, actor_schema: &str, span: Span) {
        let original = value.clone();
        let (actor, result_ty) = match &original {
            RValue::Send { actor, ty, .. } | RValue::Ask { actor, ty, .. } => {
                (actor.clone(), ty.clone())
            }
            _ => return,
        };

        let alias_ty = actor.ty();
        let alias_name = actor_schema.to_string();
        let alias_operand = Operand::Var(alias_name.clone(), alias_ty.clone());
        let inner = match original {
            RValue::Send {
                behavior,
                args,
                remote,
                ty,
                ..
            } => RValue::Send {
                actor: alias_operand,
                behavior,
                args,
                remote,
                ty,
            },
            RValue::Ask {
                behavior,
                args,
                remote,
                timeout_ms,
                ty,
                ..
            } => RValue::Ask {
                actor: alias_operand,
                behavior,
                args,
                remote,
                timeout_ms,
                ty,
            },
            _ => return,
        };

        let result_name = format!("__nominal_dispatch_result_{}", self.next_temp);
        self.next_temp += 1;

        let mut block = Body::new();
        block.push(Stmt::Let {
            name: alias_name,
            ty: alias_ty,
            value: RValue::Use(actor),
            span,
        });
        block.push(Stmt::Let {
            name: result_name.clone(),
            ty: result_ty.clone(),
            value: inner,
            span,
        });
        block.set_terminator(Terminator::Yield(Operand::Var(result_name, result_ty)));
        *value = RValue::Block(Box::new(block));
    }
}

fn actor_identity_of_operand(operand: &Operand, env: &FxHashMap<String, String>) -> Option<String> {
    match operand {
        Operand::Var(name, _) => env.get(name).cloned(),
        Operand::Literal(..) | Operand::Unit => None,
    }
}

fn actor_identity_of_rvalue(
    value: &RValue,
    env: &FxHashMap<String, String>,
    current_actor: Option<&str>,
) -> Option<String> {
    match value {
        RValue::Spawn { actor_type, .. } => Some(actor_type.clone()),
        RValue::Use(operand) => actor_identity_of_operand(operand, env),
        RValue::SelfRef(_) => current_actor.map(str::to_string),
        RValue::Block(body) => actor_identity_of_body(body, env, current_actor),
        RValue::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => {
            let then_identity = actor_identity_of_body(then_body, env, current_actor)?;
            let else_identity = actor_identity_of_body(else_body, env, current_actor)?;
            (then_identity == else_identity).then_some(then_identity)
        }
        RValue::Match { arms, .. } => {
            let mut identities = arms.iter().map(|(pattern, _, arm)| {
                let mut nested = env.clone();
                shadow_pattern(&mut nested, pattern);
                actor_identity_of_body(arm, &nested, current_actor)
            });
            let first = identities.next()??;
            identities
                .all(|identity| identity.as_deref() == Some(first.as_str()))
                .then_some(first)
        }
        _ => None,
    }
}

fn actor_identity_of_body(
    body: &Body,
    env: &FxHashMap<String, String>,
    current_actor: Option<&str>,
) -> Option<String> {
    let mut nested = env.clone();

    for stmt in &body.stmts {
        match stmt {
            Stmt::Let { name, value, .. } => {
                let invalidated = invalidated_actor_bindings(value, &nested);
                for binding in invalidated {
                    nested.remove(&binding);
                }

                let identity = actor_identity_of_rvalue(value, &nested, current_actor);
                nested.remove(name);
                if let Some(identity) = identity {
                    nested.insert(name.clone(), identity);
                }
            }
            Stmt::Assign { target, value, .. } => {
                let invalidated = invalidated_actor_bindings(value, &nested);
                for binding in invalidated {
                    nested.remove(&binding);
                }

                if let Place::Var(name, _) = target {
                    let identity = actor_identity_of_rvalue(value, &nested, current_actor);
                    nested.remove(name);
                    if let Some(identity) = identity {
                        nested.insert(name.clone(), identity);
                    }
                }
            }
            Stmt::StateSet { .. } | Stmt::Emit { .. } => {}
        }
    }

    match &body.terminator {
        Terminator::Yield(operand) => actor_identity_of_operand(operand, &nested),
        Terminator::FnReturn(_) | Terminator::Break(_) => None,
    }
}

fn invalidated_actor_bindings(value: &RValue, env: &FxHashMap<String, String>) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_rvalue_assignments(value, env, &HashSet::new(), &mut out);
    out
}

fn collect_body_assignments(
    body: &Body,
    env: &FxHashMap<String, String>,
    inherited_shadowed: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    let mut shadowed = inherited_shadowed.clone();
    for stmt in &body.stmts {
        match stmt {
            Stmt::Let { name, value, .. } => {
                collect_rvalue_assignments(value, env, &shadowed, out);
                shadowed.insert(name.clone());
            }
            Stmt::Assign { target, value, .. } => {
                collect_rvalue_assignments(value, env, &shadowed, out);
                if let Place::Var(name, _) = target {
                    if env.contains_key(name) && !shadowed.contains(name) {
                        out.insert(name.clone());
                    }
                }
            }
            Stmt::StateSet { .. } | Stmt::Emit { .. } => {}
        }
    }
}

fn collect_rvalue_assignments(
    value: &RValue,
    env: &FxHashMap<String, String>,
    shadowed: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    match value {
        RValue::Closure { .. } | RValue::RecClosure { .. } => {}
        RValue::If {
            then_body,
            else_body,
            ..
        } => {
            collect_body_assignments(then_body, env, shadowed, out);
            if let Some(else_body) = else_body {
                collect_body_assignments(else_body, env, shadowed, out);
            }
        }
        RValue::Match { arms, .. } => {
            for (pattern, guard, arm) in arms {
                let mut nested = shadowed.clone();
                shadow_pattern_names(&mut nested, pattern);
                if let Some(guard) = guard {
                    collect_body_assignments(guard, env, &nested, out);
                }
                collect_body_assignments(arm, env, &nested, out);
            }
        }
        RValue::For { var, body, .. } => {
            let mut nested = shadowed.clone();
            nested.insert(var.clone());
            collect_body_assignments(body, env, &nested, out);
        }
        RValue::While { cond, body, .. } => {
            collect_body_assignments(cond, env, shadowed, out);
            collect_body_assignments(body, env, shadowed, out);
        }
        RValue::Block(body) => collect_body_assignments(body, env, shadowed, out),
        RValue::Handle { body, handlers, .. } => {
            collect_body_assignments(body, env, shadowed, out);
            for handler in handlers {
                let mut nested = shadowed.clone();
                for (name, _) in &handler.params {
                    nested.insert(name.clone());
                }
                collect_body_assignments(&handler.body, env, &nested, out);
            }
        }
        RValue::Receive { arms, after, .. } => {
            for (_, patterns, guard, arm) in arms {
                let mut nested = shadowed.clone();
                for pattern in patterns {
                    shadow_pattern_names(&mut nested, pattern);
                }
                if let Some(guard) = guard {
                    collect_body_assignments(guard, env, &nested, out);
                }
                collect_body_assignments(arm, env, &nested, out);
            }
            if let Some((timeout, after_body)) = after {
                collect_body_assignments(timeout, env, shadowed, out);
                collect_body_assignments(after_body, env, shadowed, out);
            }
        }
        _ => {}
    }
}

fn shadow_pattern(env: &mut FxHashMap<String, String>, pattern: &Pattern) {
    match pattern {
        Pattern::Var(name) => {
            env.remove(name);
        }
        Pattern::Tuple(items) => {
            for item in items {
                shadow_pattern(env, item);
            }
        }
        Pattern::Record(fields) => {
            for (_, item) in fields {
                shadow_pattern(env, item);
            }
        }
        Pattern::Variant(_, Some(inner)) => shadow_pattern(env, inner),
        Pattern::Alias(name, inner) => {
            env.remove(name);
            shadow_pattern(env, inner);
        }
        Pattern::Wild | Pattern::Lit(_) | Pattern::Variant(_, None) => {}
    }
}

fn shadow_pattern_names(names: &mut HashSet<String>, pattern: &Pattern) {
    match pattern {
        Pattern::Var(name) => {
            names.insert(name.clone());
        }
        Pattern::Tuple(items) => {
            for item in items {
                shadow_pattern_names(names, item);
            }
        }
        Pattern::Record(fields) => {
            for (_, item) in fields {
                shadow_pattern_names(names, item);
            }
        }
        Pattern::Variant(_, Some(inner)) => shadow_pattern_names(names, inner),
        Pattern::Alias(name, inner) => {
            names.insert(name.clone());
            shadow_pattern_names(names, inner);
        }
        Pattern::Wild | Pattern::Lit(_) | Pattern::Variant(_, None) => {}
    }
}