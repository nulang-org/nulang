#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def replace_between(text: str, start: str, end: str, replacement: str, label: str) -> str:
    start_pos = text.find(start)
    if start_pos < 0:
        raise SystemExit(f"{label}: start marker not found")
    end_pos = text.find(end, start_pos)
    if end_pos < 0:
        raise SystemExit(f"{label}: end marker not found")
    return text[:start_pos] + replacement + text[end_pos:]


def patch_typechecker() -> None:
    path = Path("src/typechecker.rs")
    text = path.read_text()

    text = replace_once(
        text,
        '''            Expr::Ask {
                actor,
                behavior: _,
                args: _,
                span,
                ..
            } => self.infer_ask(ctx, actor, *span),''',
        '''            Expr::Ask {
                actor,
                behavior,
                args,
                span,
                ..
            } => self.infer_ask(ctx, actor, behavior, args, *span),''',
        "ask inference dispatch",
    )

    helper_marker = "    fn infer_actor_decl(\n"
    helper = '''    fn pack_behavior_params(mut params: Vec<Type>) -> Type {
        match params.len() {
            0 => Type::Tuple(Vec::new()),
            1 => params.remove(0),
            _ => Type::Tuple(params),
        }
    }

    /// Resolve a statically-known actor behavior contract from the receiver's
    /// nominal behavior schema. `None` means the receiver is intentionally
    /// opaque/dynamic and cannot be checked further here.
    fn actor_behavior_signature(
        &self,
        actor_ty: &Type,
        behavior_name: &str,
        span: Span,
    ) -> NuResult<Option<(Type, Type)>> {
        let Type::Actor { behavior, .. } = actor_ty else {
            return Err(NuError::type_error(
                format!("message receiver is not an actor: {}", actor_ty),
                span,
            ));
        };

        let (owner, schema) = match behavior.as_ref() {
            Type::Nominal { name, underlying } => (Some(name.as_str()), underlying.as_ref()),
            Type::Var(_) => return Ok(None),
            other => (None, other),
        };

        let Type::Record(fields) = schema else {
            return Ok(None);
        };
        let Some((_, signature)) = fields.iter().find(|(name, _)| name == behavior_name) else {
            let owner = owner
                .and_then(|name| name.strip_prefix("actor::"))
                .and_then(|name| name.strip_suffix("::behaviors"))
                .unwrap_or("actor");
            return Err(NuError::type_error(
                format!("Actor '{}' does not declare behavior '{}'", owner, behavior_name),
                span,
            ));
        };

        match signature {
            Type::Function { param, ret, .. } => Ok(Some(((**param).clone(), (**ret).clone()))),
            other => Err(NuError::type_error(
                format!(
                    "internal: behavior '{}' has non-function signature {}",
                    behavior_name, other
                ),
                span,
            )),
        }
    }

'''
    if helper_marker not in text:
        raise SystemExit("infer_actor_decl marker not found")
    text = text.replace(helper_marker, helper + helper_marker, 1)

    text = replace_between(
        text,
        "        // The actor's own name must be in scope inside its behaviors so an\n",
        "        // Type-check state field defaults and CRDT type compatibility.\n",
        '''        // Materialize one receiver-specific behavior contract before checking
        // behavior bodies. The existing Actor.behavior slot is the semantic
        // home for this identity; the nominal owner prevents same-short-name
        // handlers on different actors from unifying accidentally.
        let behavior_signatures: Vec<(String, Vec<Type>, Type, EffectRow, Capability)> = behaviors
            .iter()
            .map(|behavior| {
                let params = behavior
                    .params
                    .iter()
                    .map(|param| {
                        param
                            .ty
                            .clone()
                            .unwrap_or_else(|| Type::Var(TypeVar::fresh()))
                    })
                    .collect::<Vec<_>>();
                let ret = behavior
                    .ret_type
                    .clone()
                    .unwrap_or_else(|| Type::Var(TypeVar::fresh()));
                (
                    behavior.name.clone(),
                    params,
                    ret,
                    behavior.effect.clone().unwrap_or_else(EffectRow::empty),
                    behavior.cap,
                )
            })
            .collect();
        let behavior_record = Type::Record(
            behavior_signatures
                .iter()
                .map(|(name, params, ret, effect, cap)| {
                    (
                        name.clone(),
                        Type::Function {
                            param: Box::new(Self::pack_behavior_params(params.clone())),
                            ret: Box::new(ret.clone()),
                            effect: effect.clone(),
                            cap: *cap,
                        },
                    )
                })
                .collect(),
        );
        let behavior_identity = Type::Nominal {
            name: format!("actor::{}::behaviors", name),
            underlying: Box::new(behavior_record),
        };
        let self_ty = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(behavior_identity.clone()),
        };

''',
        "actor behavior schema",
    )

    text = replace_between(
        text,
        "        // Check each behavior, with event declarations in scope for emit checking\n",
        "        // Typecheck migration contracts: state_body and event_migration handlers\n",
        '''        // Check each behavior against the same signature variables embedded
        // in the actor type. This lets an unannotated return type be inferred
        // once and then flow to every `ask` site for this actor.
        let mut behavior_subst = Substitution::new();
        for (behavior_index, behavior) in behaviors.iter().enumerate() {
            let mut behavior_ctx = apply_subst_to_ctx(ctx, &behavior_subst);
            behavior_ctx.bind(
                name.to_string(),
                apply_subst(&self_ty, &behavior_subst),
                Capability::Ref,
                false,
            );
            let (_, param_types, ret_type, _, _) = &behavior_signatures[behavior_index];
            for (param, param_ty) in behavior.params.iter().zip(param_types.iter()) {
                behavior_ctx.bind(
                    param.name.clone(),
                    apply_subst(param_ty, &behavior_subst),
                    param.cap.unwrap_or(Capability::Ref),
                    false,
                );
            }
            if !events.is_empty() {
                let ctx_events: Vec<(String, Vec<(String, Type)>)> = events
                    .iter()
                    .map(|e| (e.name.clone(), e.params.clone()))
                    .collect();
                behavior_ctx.set_entity_events(ctx_events);
            }
            let (s_body, body_ty) = self.infer_expr(&behavior_ctx, &behavior.body)?;
            let s_body = compose_subst(&s_body, &behavior_subst);
            let inferred_ret = apply_subst(&body_ty, &s_body);
            let expected_ret = apply_subst(ret_type, &s_body);
            let s_ret = mgu(&expected_ret, &inferred_ret, behavior.span)?;
            behavior_subst = compose_subst(&s_ret, &s_body);
        }

''',
        "behavior body typing",
    )

    text = replace_once(
        text,
        '''        let actor_ty = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        Ok((vec![], actor_ty))
    }

    /// Infer spawn expression.''',
        '''        let actor_ty = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(apply_subst(&behavior_identity, &behavior_subst)),
        };
        Ok((behavior_subst, actor_ty))
    }

    /// Infer spawn expression.''',
        "final actor type",
    )

    text = replace_between(
        text,
        "    /// Infer send expression.\n",
        "    /// Infer perform expression.\n",
        '''    /// Infer send expression.
    fn infer_send(
        &mut self,
        ctx: &TypeContext,
        actor: &Expr,
        behavior: &str,
        args: &[Expr],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, actor_ty) = self.infer_expr(ctx, actor)?;
        let fresh_actor = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        let s2 = mgu(&apply_subst(&actor_ty, &s1), &fresh_actor, span)?;
        let mut subst = compose_subst(&s2, &s1);
        let resolved_actor = apply_subst(&actor_ty, &subst);
        let signature = self.actor_behavior_signature(&resolved_actor, behavior, span)?;

        let mut arg_types = Vec::with_capacity(args.len());
        for arg in args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_arg, arg_ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s_arg, &subst);
            arg_types.push(arg_ty);
        }

        if let Some((expected_param, _)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
        }

        Ok((subst, Type::unit()))
    }

    /// Infer ask expression.
    fn infer_ask(
        &mut self,
        ctx: &TypeContext,
        actor: &Expr,
        behavior: &str,
        args: &[Expr],
        span: Span,
    ) -> NuResult<(Substitution, Type)> {
        let (s1, actor_ty) = self.infer_expr(ctx, actor)?;
        let fresh_actor = Type::Actor {
            state: Box::new(Type::Var(TypeVar::fresh())),
            behavior: Box::new(Type::Var(TypeVar::fresh())),
        };
        let s2 = mgu(&apply_subst(&actor_ty, &s1), &fresh_actor, span)?;
        let mut subst = compose_subst(&s2, &s1);
        let resolved_actor = apply_subst(&actor_ty, &subst);
        let signature = self.actor_behavior_signature(&resolved_actor, behavior, span)?;

        let mut arg_types = Vec::with_capacity(args.len());
        for arg in args {
            let ctx_sub = apply_subst_to_ctx(ctx, &subst);
            let (s_arg, arg_ty) = self.infer_expr(&ctx_sub, arg)?;
            subst = compose_subst(&s_arg, &subst);
            arg_types.push(arg_ty);
        }

        if let Some((expected_param, ret_type)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
            return Ok((subst.clone(), apply_subst(&ret_type, &subst)));
        }

        Ok((subst, Type::Var(TypeVar::fresh())))
    }

''',
        "send/ask typing",
    )

    nominal_marker = "/// Compute the most general unifier of two types.\n"
    contains_nominal = '''fn contains_nominal(ty: &Type) -> bool {
    match ty {
        Type::Nominal { .. } => true,
        Type::Tuple(types) => types.iter().any(contains_nominal),
        Type::Record(fields) => fields.iter().any(|(_, ty)| contains_nominal(ty)),
        Type::Variant(cases) => cases
            .iter()
            .any(|(_, payload)| payload.as_ref().is_some_and(contains_nominal)),
        Type::Array(inner) => contains_nominal(inner),
        Type::Function { param, ret, .. } => contains_nominal(param) || contains_nominal(ret),
        Type::Actor { state, behavior } => contains_nominal(state) || contains_nominal(behavior),
        Type::App { constructor, args } => {
            contains_nominal(constructor) || args.iter().any(contains_nominal)
        }
        Type::Reference { inner, .. } => contains_nominal(inner),
        Type::Scheme { body, .. } => contains_nominal(body),
        _ => false,
    }
}

'''
    if nominal_marker not in text:
        raise SystemExit("mgu marker not found")
    text = text.replace(nominal_marker, contains_nominal + nominal_marker, 1)

    text = replace_once(
        text,
        '''    if !matches!(t1, Type::Nominal { .. })
        && !matches!(t2, Type::Nominal { .. })
        && t1.is_ground()
        && t2.is_ground()''',
        '''    if !matches!(t1, Type::Nominal { .. })
        && !matches!(t2, Type::Nominal { .. })
        && !contains_nominal(t1)
        && !contains_nominal(t2)
        && t1.is_ground()
        && t2.is_ground()''',
        "nested nominal unification guard",
    )

    path.write_text(text)


def patch_mir_lower() -> None:
    path = Path("src/mir_lower.rs")
    text = path.read_text()

    text = replace_between(
        text,
        "    /// Resolve `send`/`ask actor behavior(...)` to a behavior-table index by\n",
        "    fn fresh_lambda_name(&mut self) -> String {\n",
        '''    /// Resolve send/ask only from the receiver's nominal actor owner.
    /// Global suffix fallback is forbidden because different actors may expose
    /// the same short behavior name.
    fn send_behavior_idx(&self, actor_name_hint: &str, behavior: &str) -> Option<usize> {
        if actor_name_hint.is_empty() {
            return None;
        }
        let full_name = format!("{}.{}", actor_name_hint, behavior);
        self.behavior_names.iter().position(|name| *name == full_name)
    }

    /// Receive arms do not yet carry an explicit actor owner in HIR. Keep the
    /// legacy suffix lookup isolated here instead of sharing it with outbound
    /// send/ask resolution.
    fn receive_behavior_idx(&self, behavior: &str) -> usize {
        let suffix = format!(".{}", behavior);
        self.behavior_names
            .iter()
            .position(|name| name.ends_with(&suffix))
            .unwrap_or(self.behaviors.len())
    }

''',
        "behavior index resolver",
    )

    old_lookup = '''                let actor_hint = operand_name_hint(actor);
                let idx = self.ctx.send_behavior_idx(&actor_hint, behavior);'''
    new_lookup = '''                let actor_hint = operand_name_hint(actor);
                let idx = self
                    .ctx
                    .send_behavior_idx(&actor_hint, behavior)
                    .ok_or_else(|| {
                        compile_err(
                            format!(
                                "cannot resolve behavior '{}.{}' from receiver type",
                                actor_hint, behavior
                            ),
                            Span::default(),
                        )
                    })?;'''
    if text.count(old_lookup) != 2:
        raise SystemExit(
            f"send/ask MIR lookup: expected two matches, found {text.count(old_lookup)}"
        )
    text = text.replace(old_lookup, new_lookup)

    text = replace_once(
        text,
        '''            .map(|(name, _, _, _)| self.ctx.send_behavior_idx("", name) as u16)''',
        '''            .map(|(name, _, _, _)| self.ctx.receive_behavior_idx(name) as u16)''',
        "receive behavior lookup",
    )

    text = replace_between(
        text,
        "/// Best-effort actor-type-name hint for `send`/`ask` behavior resolution,\n",
        "\nfn literal_to_constant(",
        '''/// Recover the actor declaration owner from the nominal behavior schema
/// installed by the typechecker. Variable spelling is not semantic identity.
fn nominal_actor_name_from_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Actor { behavior, .. } => match behavior.as_ref() {
            Type::Nominal { name, .. } => name
                .strip_prefix("actor::")
                .and_then(|name| name.strip_suffix("::behaviors"))
                .map(str::to_owned),
            _ => None,
        },
        Type::Scheme { body, .. } => nominal_actor_name_from_type(body),
        _ => None,
    }
}

fn operand_name_hint(op: &hir::Operand) -> String {
    let ty = match op {
        hir::Operand::Var(_, ty) | hir::Operand::Literal(_, ty) => Some(ty),
        hir::Operand::Unit => None,
    };
    if let Some(actor_name) = ty.and_then(nominal_actor_name_from_type) {
        return actor_name;
    }
    match op {
        hir::Operand::Var(name, _) => name.clone(),
        _ => String::new(),
    }
}
''',
        "typed actor owner hint",
    )

    path.write_text(text)


if __name__ == "__main__":
    patch_typechecker()
    patch_mir_lower()
