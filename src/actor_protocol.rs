//! Static actor-protocol annotation and validation.
//!
//! This pass is intentionally additive: it uses actor declarations that are
//! statically known in the current module to constrain `send`/`ask` with the
//! existing `Expr::TypeAnnotate` node. The ordinary HM typechecker remains the
//! source of truth for unification. Dynamic/opaque actor references retain the
//! previous permissive behavior until an explicit protocol type is available.

use crate::ast::{self, AstModule, Behavior, Decl, Expr, Literal, Param, Pattern, StateMachineEvent};
use crate::types::{NuError, NuResult, PrimitiveType, Span, Type};
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
struct BehaviorSig {
    params: Vec<Option<Type>>,
    ret: Option<Type>,
}

#[derive(Debug, Clone, Default)]
struct ActorProtocol {
    behaviors: FxHashMap<String, BehaviorSig>,
}

#[derive(Debug, Clone, Default)]
struct Env {
    actors: FxHashMap<String, String>,
    current_actor: Option<String>,
}

/// Clone a parsed module and enrich statically resolvable actor calls with type
/// annotations. Unknown actor identities are left untouched for compatibility.
pub fn annotate_module(module: &AstModule) -> NuResult<AstModule> {
    let mut protocols = FxHashMap::default();
    collect_protocols(&module.decls, &mut protocols);

    let mut annotated = module.clone();
    let mut env = Env::default();
    annotate_decls(&mut annotated.decls, &mut env, &protocols)?;
    Ok(annotated)
}

fn collect_protocols(decls: &[Decl], out: &mut FxHashMap<String, ActorProtocol>) {
    for decl in decls {
        match decl {
            Decl::Actor {
                name,
                state_fields,
                behaviors,
                ..
            } => {
                out.insert(
                    name.clone(),
                    protocol_from_behaviors(behaviors, state_fields),
                );
            }
            Decl::StateMachine {
                name,
                states,
                events,
                entry_hooks,
                exit_hooks,
                span,
            } => {
                let desugared = ast::desugar_state_machine(
                    name,
                    states,
                    events,
                    entry_hooks,
                    exit_hooks,
                    *span,
                );
                if let Decl::Actor {
                    state_fields,
                    behaviors,
                    ..
                } = desugared
                {
                    out.insert(
                        name.clone(),
                        protocol_from_behaviors(&behaviors, &state_fields),
                    );
                }
            }
            Decl::Module { decls, .. } => collect_protocols(decls, out),
            _ => {}
        }
    }
}

fn protocol_from_behaviors(
    behaviors: &[Behavior],
    state_fields: &[(String, ast::StateModel, Type, Expr)],
) -> ActorProtocol {
    let mut protocol = ActorProtocol::default();
    for behavior in behaviors {
        let params = behavior.params.iter().map(|p| p.ty.clone()).collect();
        let ret = behavior
            .ret_type
            .clone()
            .or_else(|| infer_simple_return_type(&behavior.body, state_fields));
        protocol
            .behaviors
            .insert(behavior.name.clone(), BehaviorSig { params, ret });
    }
    protocol
}

/// Conservative return inference for the common actor-query cases. If the
/// shape is not obvious, return `None` and preserve the old unconstrained ask
/// result rather than inventing a type.
fn infer_simple_return_type(
    expr: &Expr,
    state_fields: &[(String, ast::StateModel, Type, Expr)],
) -> Option<Type> {
    match expr {
        Expr::Literal(lit, _) => Some(match lit {
            Literal::Int(_) => Type::int(),
            Literal::Float(_) => Type::float(),
            Literal::String(_) => Type::string(),
            Literal::Bool(_) => Type::bool(),
            Literal::Nil => Type::nil(),
            Literal::Unit => Type::unit(),
        }),
        Expr::TypeAnnotate { ty, .. } => Some(ty.clone()),
        Expr::FieldAccess { expr, field, .. } if matches!(expr.as_ref(), Expr::SelfRef(_)) => {
            state_fields
                .iter()
                .find(|(name, _, _, _)| name == field)
                .map(|(_, _, ty, _)| ty.clone())
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => exprs
            .last()
            .and_then(|last| infer_simple_return_type(last, state_fields)),
        Expr::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => {
            let a = infer_simple_return_type(then_branch, state_fields)?;
            let b = infer_simple_return_type(else_branch, state_fields)?;
            (a == b).then_some(a)
        }
        Expr::Tuple(items, _) => {
            let mut types = Vec::with_capacity(items.len());
            for item in items {
                types.push(infer_simple_return_type(item, state_fields)?);
            }
            Some(Type::Tuple(types))
        }
        Expr::Array(items, _) if !items.is_empty() => {
            let first = infer_simple_return_type(&items[0], state_fields)?;
            if items
                .iter()
                .skip(1)
                .all(|item| infer_simple_return_type(item, state_fields).as_ref() == Some(&first))
            {
                Some(Type::Array(Box::new(first)))
            } else {
                None
            }
        }
        Expr::Binary { op, left, right, .. } => {
            use ast::BinOp::*;
            match op {
                Eq | Ne | Lt | Le | Gt | Ge | And | Or => Some(Type::bool()),
                Add | Sub | Mul | Div | Mod | Pow => {
                    let left_ty = infer_simple_return_type(left, state_fields)?;
                    let right_ty = infer_simple_return_type(right, state_fields)?;
                    if left_ty == right_ty
                        && matches!(
                            left_ty,
                            Type::Primitive(PrimitiveType::Int)
                                | Type::Primitive(PrimitiveType::Float)
                                | Type::Primitive(PrimitiveType::String)
                        )
                    {
                        Some(left_ty)
                    } else {
                        None
                    }
                }
                BitAnd | BitOr | BitXor | Shl | Shr => Some(Type::int()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn set_actor_binding(
    env: &mut Env,
    name: &str,
    actor_name: Option<String>,
    protocols: &FxHashMap<String, ActorProtocol>,
) {
    // Every lexical binder shadows the outer name, even when the new value is
    // not a statically known actor. Keeping a stale actor entry here would make
    // the protocol pass validate a new binding against an unrelated outer
    // actor's protocol.
    env.actors.remove(name);
    if let Some(actor_name) = actor_name.filter(|name| protocols.contains_key(name)) {
        env.actors.insert(name.to_string(), actor_name);
    }
}

fn shadow_params(env: &mut Env, params: &[Param]) {
    for param in params {
        env.actors.remove(&param.name);
    }
}

fn shadow_pattern(env: &mut Env, pattern: &Pattern) {
    match pattern {
        Pattern::Var(name) => {
            env.actors.remove(name);
        }
        Pattern::Tuple(items) | Pattern::Record(items) => {
            match pattern {
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
                _ => unreachable!(),
            }
        }
        Pattern::Variant(_, Some(inner)) => shadow_pattern(env, inner),
        Pattern::Alias(name, inner) => {
            env.actors.remove(name);
            shadow_pattern(env, inner);
        }
        Pattern::Wild | Pattern::Lit(_) | Pattern::Variant(_, None) => {}
    }
}

fn annotate_decls(
    decls: &mut [Decl],
    env: &mut Env,
    protocols: &FxHashMap<String, ActorProtocol>,
) -> NuResult<()> {
    for decl in decls {
        annotate_decl(decl, env, protocols)?;
        match decl {
            Decl::LetBinding { name, value, .. } => {
                let actor_name = actor_name_for_expr(value, env);
                set_actor_binding(env, name, actor_name, protocols);
            }
            Decl::Signal { name, init, .. } => {
                let actor_name = actor_name_for_expr(init, env);
                set_actor_binding(env, name, actor_name, protocols);
            }
            _ => {}
        }
    }
    Ok(())
}

fn annotate_decl(
    decl: &mut Decl,
    env: &Env,
    protocols: &FxHashMap<String, ActorProtocol>,
) -> NuResult<()> {
    match decl {
        Decl::Function {
            name,
            params,
            default_values,
            using_params,
            requires,
            ensures,
            body,
            ..
        } => {
            for default in default_values.iter_mut().flatten() {
                annotate_expr(default, env, protocols)?;
            }
            let mut fn_env = env.clone();
            fn_env.actors.remove(name);
            shadow_params(&mut fn_env, params);
            shadow_params(&mut fn_env, using_params);
            for requirement in requires {
                annotate_expr(requirement, &fn_env, protocols)?;
            }
            for guarantee in ensures {
                annotate_expr(guarantee, &fn_env, protocols)?;
            }
            annotate_expr(body, &fn_env, protocols)
        }
        Decl::Actor {
            name,
            state_fields,
            behaviors,
            init,
            initializer,
            ..
        } => {
            let mut actor_env = env.clone();
            actor_env.current_actor = Some(name.clone());
            for (_, _, _, default) in state_fields {
                annotate_expr(default, &actor_env, protocols)?;
            }
            for (_, value) in init {
                annotate_expr(value, &actor_env, protocols)?;
            }
            if let Some((_, params, body)) = initializer {
                let mut initializer_env = actor_env.clone();
                shadow_params(&mut initializer_env, params);
                annotate_expr(body, &initializer_env, protocols)?;
            }
            for behavior in behaviors {
                let mut behavior_env = actor_env.clone();
                shadow_params(&mut behavior_env, &behavior.params);
                annotate_expr(&mut behavior.body, &behavior_env, protocols)?;
            }
            Ok(())
        }
        Decl::StateMachine {
            name,
            entry_hooks,
            exit_hooks,
            ..
        } => {
            let mut actor_env = env.clone();
            actor_env.current_actor = Some(name.clone());
            for (_, body) in entry_hooks {
                annotate_expr(body, &actor_env, protocols)?;
            }
            for (_, body) in exit_hooks {
                annotate_expr(body, &actor_env, protocols)?;
            }
            Ok(())
        }
        Decl::LetBinding { value, .. } => annotate_expr(value, env, protocols),
        Decl::Signal { init, .. } => annotate_expr(init, env, protocols),
        Decl::Module { decls, .. } => {
            let mut module_env = env.clone();
            annotate_decls(decls, &mut module_env, protocols)
        }
        Decl::Workflow { input, items, compensate, .. } => {
            let mut workflow_env = env.clone();
            if let Some((name, _)) = input {
                workflow_env.actors.remove(name);
            }
            for item in items {
                match item {
                    ast::WorkflowItem::Step(step) => {
                        annotate_expr(&mut step.body, &workflow_env, protocols)?;
                        if let Some(compensate) = &mut step.compensate {
                            annotate_expr(compensate, &workflow_env, protocols)?;
                        }
                    }
                    ast::WorkflowItem::Parallel(steps) => {
                        for step in steps {
                            annotate_expr(&mut step.body, &workflow_env, protocols)?;
                            if let Some(compensate) = &mut step.compensate {
                                annotate_expr(compensate, &workflow_env, protocols)?;
                            }
                        }
                    }
                }
            }
            if let Some(compensate) = compensate {
                annotate_expr(compensate, &workflow_env, protocols)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn actor_name_for_expr(expr: &Expr, env: &Env) -> Option<String> {
    match expr {
        Expr::Var(name, _) => env.actors.get(name).cloned(),
        Expr::SelfRef(_) => env.current_actor.clone(),
        Expr::Spawn { actor_type, .. } => match actor_type.as_ref() {
            Expr::Var(name, _) => Some(name.clone()),
            _ => None,
        },
        Expr::GrainRef { grain_type, .. } => Some(grain_type.clone()),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            actor_name_for_expr(expr, env)
        }
        _ => None,
    }
}

fn validate_call(
    actor: &Expr,
    behavior: &str,
    args: &mut [Expr],
    env: &Env,
    protocols: &FxHashMap<String, ActorProtocol>,
    span: Span,
) -> NuResult<Option<Type>> {
    let Some(actor_name) = actor_name_for_expr(actor, env) else {
        return Ok(None);
    };
    let Some(protocol) = protocols.get(&actor_name) else {
        return Ok(None);
    };
    let Some(sig) = protocol.behaviors.get(behavior) else {
        let mut available: Vec<String> = protocol.behaviors.keys().cloned().collect();
        available.sort();
        return Err(NuError::TypeError {
            msg: format!(
                "actor '{}' has no behavior '{}'; available behaviors: {}",
                actor_name,
                behavior,
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            ),
            span,
            expected_type: Some("declared actor behavior".to_string()),
            found_type: Some(behavior.to_string()),
            similar_names: (!available.is_empty()).then_some(available),
        });
    };

    if args.len() != sig.params.len() {
        return Err(NuError::TypeError {
            msg: format!(
                "behavior '{}.{}' expects {} argument(s), got {}",
                actor_name,
                behavior,
                sig.params.len(),
                args.len()
            ),
            span,
            expected_type: Some(format!("{} argument(s)", sig.params.len())),
            found_type: Some(format!("{} argument(s)", args.len())),
            similar_names: None,
        });
    }

    for (arg, expected) in args.iter_mut().zip(sig.params.iter()) {
        let Some(expected) = expected else {
            continue;
        };
        let arg_span = arg.span();
        let placeholder = Expr::Literal(Literal::Unit, arg_span);
        let original = std::mem::replace(arg, placeholder);
        *arg = Expr::TypeAnnotate {
            expr: Box::new(original),
            ty: expected.clone(),
            span: arg_span,
        };
    }

    Ok(sig.ret.clone())
}

fn annotate_expr(
    expr: &mut Expr,
    env: &Env,
    protocols: &FxHashMap<String, ActorProtocol>,
) -> NuResult<()> {
    match expr {
        Expr::Literal(..) | Expr::Var(..) | Expr::SelfRef(..) | Expr::Panic(..) => Ok(()),
        Expr::FString(parts, _) | Expr::Tuple(parts, _) | Expr::Array(parts, _) => {
            for part in parts {
                annotate_expr(part, env, protocols)?;
            }
            Ok(())
        }
        Expr::Lambda { params, body, .. } => {
            let mut lambda_env = env.clone();
            shadow_params(&mut lambda_env, params);
            annotate_expr(body, &lambda_env, protocols)
        }
        Expr::App { func, args, .. } => {
            annotate_expr(func, env, protocols)?;
            for arg in args {
                annotate_expr(arg, env, protocols)?;
            }
            Ok(())
        }
        Expr::Let {
            name,
            value,
            body,
            ..
        } => {
            annotate_expr(value, env, protocols)?;
            let mut body_env = env.clone();
            let actor_name = actor_name_for_expr(value, env);
            set_actor_binding(&mut body_env, name, actor_name, protocols);
            annotate_expr(body, &body_env, protocols)
        }
        Expr::LetRec {
            name,
            params,
            value,
            body,
            ..
        } => {
            let mut rec_env = env.clone();
            rec_env.actors.remove(name);
            shadow_params(&mut rec_env, params);
            annotate_expr(value, &rec_env, protocols)?;
            annotate_expr(body, &rec_env, protocols)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            annotate_expr(cond, env, protocols)?;
            annotate_expr(then_branch, env, protocols)?;
            if let Some(branch) = else_branch {
                annotate_expr(branch, env, protocols)?;
            }
            Ok(())
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            annotate_expr(scrutinee, env, protocols)?;
            for (pattern, guard, body) in arms {
                let mut arm_env = env.clone();
                shadow_pattern(&mut arm_env, pattern);
                if let Some(guard) = guard {
                    annotate_expr(guard, &arm_env, protocols)?;
                }
                annotate_expr(body, &arm_env, protocols)?;
            }
            Ok(())
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for item in exprs {
                annotate_expr(item, env, protocols)?;
            }
            Ok(())
        }
        Expr::Record(fields, _) => {
            for (_, value) in fields {
                annotate_expr(value, env, protocols)?;
            }
            Ok(())
        }
        Expr::FieldAccess { expr, .. } => annotate_expr(expr, env, protocols),
        Expr::RecordUpdate { base, fields, .. } => {
            annotate_expr(base, env, protocols)?;
            for (_, value) in fields {
                annotate_expr(value, env, protocols)?;
            }
            Ok(())
        }
        Expr::Index { arr, idx, .. } => {
            annotate_expr(arr, env, protocols)?;
            annotate_expr(idx, env, protocols)
        }
        Expr::Binary { left, right, .. } => {
            annotate_expr(left, env, protocols)?;
            annotate_expr(right, env, protocols)
        }
        Expr::Unary { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::Recover { body: expr, .. }
        | Expr::Defer { expr, .. } => annotate_expr(expr, env, protocols),
        Expr::Hide { names, body, .. } => {
            let mut hidden_env = env.clone();
            for name in names {
                hidden_env.actors.remove(name);
            }
            annotate_expr(body, &hidden_env, protocols)
        }
        Expr::Seal { names, body, .. } => {
            let mut sealed_env = env.clone();
            sealed_env.actors.retain(|name, _| names.contains(name));
            annotate_expr(body, &sealed_env, protocols)
        }
        Expr::Assign { target, value, .. } => {
            annotate_expr(target, env, protocols)?;
            annotate_expr(value, env, protocols)
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            annotate_expr(actor_type, env, protocols)?;
            for (_, value) in init {
                annotate_expr(value, env, protocols)?;
            }
            if let Some(args) = positional_args {
                for arg in args {
                    annotate_expr(arg, env, protocols)?;
                }
            }
            if let Some(node) = target_node {
                annotate_expr(node, env, protocols)?;
            }
            Ok(())
        }
        Expr::Send {
            actor,
            behavior,
            args,
            span,
            ..
        } => {
            annotate_expr(actor, env, protocols)?;
            for arg in args.iter_mut() {
                annotate_expr(arg, env, protocols)?;
            }
            let _ = validate_call(actor, behavior, args, env, protocols, *span)?;
            Ok(())
        }
        Expr::Ask {
            actor,
            behavior,
            args,
            span,
            ..
        } => {
            annotate_expr(actor, env, protocols)?;
            for arg in args.iter_mut() {
                annotate_expr(arg, env, protocols)?;
            }
            let ask_span = *span;
            let ret = validate_call(actor, behavior, args, env, protocols, ask_span)?;
            if let Some(ret) = ret {
                let placeholder = Expr::Literal(Literal::Unit, ask_span);
                let inner = std::mem::replace(expr, placeholder);
                *expr = Expr::TypeAnnotate {
                    expr: Box::new(inner),
                    ty: ret,
                    span: ask_span,
                };
            }
            Ok(())
        }
        Expr::Receive { arms, after, .. } => {
            for (_, patterns, guard, body) in arms {
                let mut arm_env = env.clone();
                for pattern in patterns {
                    shadow_pattern(&mut arm_env, pattern);
                }
                if let Some(guard) = guard {
                    annotate_expr(guard, &arm_env, protocols)?;
                }
                annotate_expr(body, &arm_env, protocols)?;
            }
            if let Some((timeout, body)) = after {
                annotate_expr(timeout, env, protocols)?;
                annotate_expr(body, env, protocols)?;
            }
            Ok(())
        }
        Expr::Emit { args, .. } | Expr::Perform { args, .. } => {
            for arg in args {
                annotate_expr(arg, env, protocols)?;
            }
            Ok(())
        }
        Expr::GrainRef { key, .. } => annotate_expr(key, env, protocols),
        Expr::Resume { value, .. } => annotate_expr(value, env, protocols),
        Expr::Handle { body, handlers, .. } => {
            annotate_expr(body, env, protocols)?;
            for handler in handlers {
                let mut handler_env = env.clone();
                for param in &handler.params {
                    handler_env.actors.remove(param);
                }
                annotate_expr(&mut handler.body, &handler_env, protocols)?;
            }
            Ok(())
        }
        Expr::Migrate { actor, node, .. } => {
            annotate_expr(actor, env, protocols)?;
            annotate_expr(node, env, protocols)
        }
        Expr::Pipe { left, right, .. } => {
            annotate_expr(left, env, protocols)?;
            annotate_expr(right, env, protocols)
        }
        Expr::For {
            var,
            iterable,
            body,
            ..
        } => {
            annotate_expr(iterable, env, protocols)?;
            let mut body_env = env.clone();
            body_env.actors.remove(var);
            annotate_expr(body, &body_env, protocols)
        }
        Expr::While { cond, body, .. } => {
            annotate_expr(cond, env, protocols)?;
            annotate_expr(body, env, protocols)
        }
        Expr::Return(value, _) | Expr::Break(value, _) => {
            if let Some(value) = value {
                annotate_expr(value, env, protocols)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(src: &str) -> AstModule {
        let tokens = Lexer::new(src).lex().expect("lex");
        Parser::new(tokens).parse_module().expect("parse")
    }

    fn check(src: &str) -> NuResult<Type> {
        let ast = parse(src);
        let annotated = annotate_module(&ast)?;
        let mut tc = crate::typechecker_base::TypeChecker::new();
        tc.check_module(&annotated)
    }

    #[test]
    fn rejects_unknown_behavior_on_known_actor() {
        let result = check(
            r#"
            actor Counter { behavior inc() { nil } }
            let c = spawn Counter {} in send c typo()
            "#,
        );
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("typo"));
    }

    #[test]
    fn rejects_wrong_behavior_arity() {
        let result = check(
            r#"
            actor Counter { behavior add(x: Int) { nil } }
            let c = spawn Counter {} in send c add()
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_wrong_behavior_argument_type() {
        let result = check(
            r#"
            actor Counter { behavior add(x: Int) { nil } }
            let c = spawn Counter {} in send c add("bad")
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn ask_uses_inferred_state_field_return_type() {
        let result = check(
            r#"
            actor Counter {
                state n: Int = 0
                behavior get() { self.n }
            }
            fn main() -> String {
                let c = spawn Counter {} in ask c get()
            }
            "#,
        );
        assert!(result.is_err(), "Counter.get returns Int, not String");
    }

    #[test]
    fn ask_accepts_matching_return_type() {
        let result = check(
            r#"
            actor Counter {
                state n: Int = 0
                behavior get() { self.n }
            }
            fn main() -> Int {
                let c = spawn Counter {} in ask c get()
            }
            "#,
        );
        assert!(result.is_ok(), "expected typed ask to return Int: {:?}", result.err());
    }

    #[test]
    fn opaque_dynamic_actor_keeps_compatibility_fallback() {
        let result = check("fn relay(a) { send a whatever(1) }");
        assert!(result.is_ok(), "dynamic actors remain permissive: {:?}", result.err());
    }

    #[test]
    fn let_shadow_clears_outer_actor_identity() {
        let result = check(
            r#"
            actor Counter { behavior inc() { nil } }
            let c = spawn Counter {} in
                let c = other in send c whatever(1)
            "#,
        );
        assert!(result.is_ok(), "shadowed dynamic actor should stay permissive: {:?}", result.err());
    }

    #[test]
    fn function_param_shadows_module_actor_binding() {
        let result = check(
            r#"
            actor Counter { behavior inc() { nil } }
            let target = spawn Counter {}
            fn relay(target) { send target whatever(1) }
            "#,
        );
        assert!(result.is_ok(), "function parameter must shadow outer actor identity: {:?}", result.err());
    }
}