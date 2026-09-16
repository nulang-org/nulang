#!/usr/bin/env python3
from pathlib import Path
import re


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


def patch_semantic_schema() -> None:
    path = Path("src/semantic_schema.rs")
    text = path.read_text()
    marker = "#[derive(Debug, Clone, PartialEq, Eq)]\npub struct ActorStateSchema {\n"
    addition = '''#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorSchemaKey {
    pub actor_name: String,
}

impl ActorSchemaKey {
    pub fn new(actor_name: impl Into<String>) -> Self {
        Self {
            actor_name: actor_name.into(),
        }
    }
}

/// Compiler-owned nominal identity for one actor behavior. Backends may lower
/// this to compact local slots, but semantic passes must not reconstruct it
/// from source-variable spelling or global suffix matching.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BehaviorKey {
    pub actor: ActorSchemaKey,
    pub behavior_name: String,
}

impl BehaviorKey {
    pub fn new(actor_name: impl Into<String>, behavior_name: impl Into<String>) -> Self {
        Self {
            actor: ActorSchemaKey::new(actor_name),
            behavior_name: behavior_name.into(),
        }
    }

    pub fn from_nominal_behavior_schema(
        nominal_name: &str,
        behavior_name: &str,
    ) -> Option<Self> {
        let actor_name = nominal_name
            .strip_prefix("actor::")?
            .strip_suffix("::behaviors")?;
        Some(Self::new(actor_name, behavior_name))
    }

    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.actor.actor_name, self.behavior_name)
    }
}

'''
    if marker not in text:
        raise SystemExit("semantic schema actor marker not found")
    text = text.replace(marker, addition + marker, 1)
    path.write_text(text)


def patch_typechecker() -> None:
    path = Path("src/typechecker.rs")
    text = path.read_text()

    text = replace_once(
        text,
        '''    pub inferred_decl_types: FxHashMap<String, Type>,
    /// Contextual `given` bindings: name → (type annotation, value expression).''',
        '''    pub inferred_decl_types: FxHashMap<String, Type>,
    /// Receiver-specific actor behavior identities proven during typechecking,
    /// keyed by the send/ask expression's compact source span.
    pub resolved_behavior_keys:
        FxHashMap<(u32, u32), crate::semantic_schema::BehaviorKey>,
    /// Contextual `given` bindings: name → (type annotation, value expression).''',
        "typechecker behavior sidecar field",
    )

    text = replace_once(
        text,
        '''            inferred_decl_types: FxHashMap::default(),
            given_bindings: FxHashMap::default(),''',
        '''            inferred_decl_types: FxHashMap::default(),
            resolved_behavior_keys: FxHashMap::default(),
            given_bindings: FxHashMap::default(),''',
        "typechecker behavior sidecar init",
    )

    text = replace_once(
        text,
        '''    pub fn check_module(&mut self, module: &AstModule) -> NuResult<Type> {
        self.register_class_decls(module);''',
        '''    pub fn check_module(&mut self, module: &AstModule) -> NuResult<Type> {
        self.resolved_behavior_keys.clear();
        self.register_class_decls(module);''',
        "typechecker behavior sidecar reset",
    )

    text = replace_between(
        text,
        "    /// Resolve a statically-known actor behavior contract from the receiver's\n",
        "    fn infer_actor_decl(\n",
        '''    /// Resolve a statically-known actor behavior contract from the receiver's
    /// nominal behavior schema. `None` means the receiver is intentionally
    /// opaque/dynamic and cannot be checked further here.
    fn actor_behavior_signature(
        &self,
        actor_ty: &Type,
        behavior_name: &str,
        span: Span,
    ) -> NuResult<
        Option<(
            Option<crate::semantic_schema::BehaviorKey>,
            Type,
            Type,
        )>,
    > {
        let Type::Actor { behavior, .. } = actor_ty else {
            return Err(NuError::type_error(
                format!("message receiver is not an actor: {}", actor_ty),
                span,
            ));
        };

        let (owner_schema_name, schema) = match behavior.as_ref() {
            Type::Nominal { name, underlying } => (Some(name.as_str()), underlying.as_ref()),
            Type::Var(_) => return Ok(None),
            other => (None, other),
        };

        let Type::Record(fields) = schema else {
            return Ok(None);
        };
        let Some((_, signature)) = fields.iter().find(|(name, _)| name == behavior_name) else {
            let owner = owner_schema_name
                .and_then(|name| name.strip_prefix("actor::"))
                .and_then(|name| name.strip_suffix("::behaviors"))
                .unwrap_or("actor");
            return Err(NuError::type_error(
                format!("Actor '{}' does not declare behavior '{}'", owner, behavior_name),
                span,
            ));
        };

        let behavior_key = owner_schema_name.and_then(|name| {
            crate::semantic_schema::BehaviorKey::from_nominal_behavior_schema(name, behavior_name)
        });
        match signature {
            Type::Function { param, ret, .. } => Ok(Some((
                behavior_key,
                (**param).clone(),
                (**ret).clone(),
            ))),
            other => Err(NuError::type_error(
                format!(
                    "internal: behavior '{}' has non-function signature {}",
                    behavior_name, other
                ),
                span,
            )),
        }
    }

''',
        "typed behavior key resolution",
    )

    text = replace_once(
        text,
        '''        if let Some((expected_param, _)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
        }

        Ok((subst, Type::unit()))''',
        '''        if let Some((behavior_key, expected_param, _)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
            if let Some(key) = behavior_key {
                self.resolved_behavior_keys
                    .insert((span.start, span.end), key);
            }
        }

        Ok((subst, Type::unit()))''',
        "send behavior key recording",
    )

    text = replace_once(
        text,
        '''        if let Some((expected_param, ret_type)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
            return Ok((subst.clone(), apply_subst(&ret_type, &subst)));
        }''',
        '''        if let Some((behavior_key, expected_param, ret_type)) = signature {
            let actual_param = Self::pack_behavior_params(
                arg_types
                    .iter()
                    .map(|ty| apply_subst(ty, &subst))
                    .collect(),
            );
            let s_args = mgu(&apply_subst(&expected_param, &subst), &actual_param, span)?;
            subst = compose_subst(&s_args, &subst);
            if let Some(key) = behavior_key {
                self.resolved_behavior_keys
                    .insert((span.start, span.end), key);
            }
            return Ok((subst.clone(), apply_subst(&ret_type, &subst)));
        }''',
        "ask behavior key recording",
    )

    path.write_text(text)


def patch_hir() -> None:
    path = Path("src/hir.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''    Send {
        actor: Operand,
        behavior: String,
        args: Vec<Operand>,''',
        '''    Send {
        actor: Operand,
        behavior: String,
        /// Nominal compiler-owned identity proven by the typechecker.
        /// `None` is reserved for intentionally dynamic/opaque receivers.
        behavior_key: Option<crate::semantic_schema::BehaviorKey>,
        args: Vec<Operand>,''',
        "HIR send behavior key",
    )
    text = replace_once(
        text,
        '''    Ask {
        actor: Operand,
        behavior: String,
        args: Vec<Operand>,''',
        '''    Ask {
        actor: Operand,
        behavior: String,
        /// Nominal compiler-owned identity proven by the typechecker.
        /// `None` is reserved for intentionally dynamic/opaque receivers.
        behavior_key: Option<crate::semantic_schema::BehaviorKey>,
        args: Vec<Operand>,''',
        "HIR ask behavior key",
    )
    path.write_text(text)


def patch_hir_lower() -> None:
    path = Path("src/hir_lower.rs")
    text = path.read_text()

    text = replace_once(
        text,
        '''pub fn lower_module(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
) -> hir::Module {
    let mut module = hir::Module::new(&ast.name);''',
        '''pub fn lower_module(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
) -> hir::Module {
    lower_module_with_behavior_keys(ast, inferred_decl_types, &FxHashMap::default())
}

pub fn lower_module_with_behavior_keys(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
    resolved_behavior_keys: &FxHashMap<
        (u32, u32),
        crate::semantic_schema::BehaviorKey,
    >,
) -> hir::Module {
    let mut module = hir::Module::new(&ast.name);''',
        "HIR lower typed entry point",
    )

    text = replace_once(
        text,
        '''    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = Some(inferred_decl_types.clone());
    });

    for decl in &ast.decls {''',
        '''    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = Some(inferred_decl_types.clone());
    });
    CURRENT_RESOLVED_BEHAVIOR_KEYS.with(|cell| {
        *cell.borrow_mut() = Some(resolved_behavior_keys.clone());
    });

    for decl in &ast.decls {''',
        "HIR lower behavior sidecar install",
    )

    text = replace_once(
        text,
        '''    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = None;
    });

    module
}''',
        '''    CURRENT_INFERRED_DECL_TYPES.with(|cell| {
        *cell.borrow_mut() = None;
    });
    CURRENT_RESOLVED_BEHAVIOR_KEYS.with(|cell| {
        *cell.borrow_mut() = None;
    });

    module
}''',
        "HIR lower behavior sidecar clear",
    )

    text = replace_once(
        text,
        '''                value: hir::RValue::Send {
                    actor: aop,
                    behavior: behavior.clone(),
                    args: aops,''',
        '''                value: hir::RValue::Send {
                    actor: aop,
                    behavior: behavior.clone(),
                    behavior_key: resolved_behavior_key(*span),
                    args: aops,''',
        "HIR send key attachment",
    )

    text = replace_once(
        text,
        '''                value: hir::RValue::Ask {
                    actor: aop,
                    behavior: behavior.clone(),
                    args: aops,''',
        '''                value: hir::RValue::Ask {
                    actor: aop,
                    behavior: behavior.clone(),
                    behavior_key: resolved_behavior_key(*span),
                    args: aops,''',
        "HIR ask key attachment",
    )

    helper_marker = "fn actor_name_from_expr(expr: &Expr) -> Option<String> {\n"
    helper = '''fn resolved_behavior_key(span: Span) -> Option<crate::semantic_schema::BehaviorKey> {
    CURRENT_RESOLVED_BEHAVIOR_KEYS.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|map| map.get(&(span.start, span.end)).cloned())
    })
}

'''
    if helper_marker not in text:
        raise SystemExit("HIR actor_name_from_expr marker not found")
    text = text.replace(helper_marker, helper + helper_marker, 1)

    text = replace_once(
        text,
        '''thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_INFERRED_DECL_TYPES: RefCell<Option<FxHashMap<String, Type>>> = RefCell::new(None);
}''',
        '''thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_INFERRED_DECL_TYPES: RefCell<Option<FxHashMap<String, Type>>> = RefCell::new(None);
    #[allow(clippy::missing_const_for_thread_local)]
    static CURRENT_RESOLVED_BEHAVIOR_KEYS: RefCell<Option<FxHashMap<(u32, u32), crate::semantic_schema::BehaviorKey>>> = RefCell::new(None);
}''',
        "HIR behavior key thread local",
    )

    path.write_text(text)


def patch_mir_lower() -> None:
    path = Path("src/mir_lower.rs")
    text = path.read_text()

    text = replace_between(
        text,
        "    /// Resolve send/ask only from the receiver's nominal actor owner.\n",
        "    /// Receive arms do not yet carry an explicit actor owner in HIR. Keep the\n",
        '''    /// Resolve a typechecker-proven nominal behavior key to its exact
    /// backend-local behavior slot. Dynamic receivers deliberately use a
    /// separate globally-unique fallback so ambiguity can never depend on
    /// declaration order.
    fn send_behavior_idx(
        &self,
        behavior_key: Option<&crate::semantic_schema::BehaviorKey>,
        behavior: &str,
    ) -> Option<usize> {
        if let Some(key) = behavior_key {
            if key.behavior_name != behavior {
                return None;
            }
            return self
                .behavior_names
                .iter()
                .position(|name| *name == key.qualified_name());
        }

        let suffix = format!(".{}", behavior);
        let mut matches = self
            .behavior_names
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| name.ends_with(&suffix).then_some(idx));
        let first = matches.next()?;
        if matches.next().is_some() {
            None
        } else {
            Some(first)
        }
    }

''',
        "MIR explicit behavior resolver",
    )

    old_send = '''            hir::RValue::Send {
                actor,
                behavior,
                args,
                remote,
                ..
            } => {
                let actor_hint = operand_name_hint(actor);
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
                    })?;
                let actor_id = self.lower_operand(actor)?;'''
    new_send = '''            hir::RValue::Send {
                actor,
                behavior,
                behavior_key,
                args,
                remote,
                ..
            } => {
                let idx = self
                    .ctx
                    .send_behavior_idx(behavior_key.as_ref(), behavior)
                    .ok_or_else(|| {
                        compile_err(
                            match behavior_key {
                                Some(key) => format!(
                                    "internal: resolved behavior '{}' has no MIR slot",
                                    key.qualified_name()
                                ),
                                None => format!(
                                    "cannot resolve behavior '{}' unambiguously from dynamic receiver",
                                    behavior
                                ),
                            },
                            Span::default(),
                        )
                    })?;
                let actor_id = self.lower_operand(actor)?;'''
    text = replace_once(text, old_send, new_send, "MIR send key dispatch")

    old_ask = '''            hir::RValue::Ask {
                actor,
                behavior,
                args,
                remote,
                timeout_ms,
                ..
            } => {
                let actor_hint = operand_name_hint(actor);
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
                    })?;
                let actor_id = self.lower_operand(actor)?;'''
    new_ask = '''            hir::RValue::Ask {
                actor,
                behavior,
                behavior_key,
                args,
                remote,
                timeout_ms,
                ..
            } => {
                let idx = self
                    .ctx
                    .send_behavior_idx(behavior_key.as_ref(), behavior)
                    .ok_or_else(|| {
                        compile_err(
                            match behavior_key {
                                Some(key) => format!(
                                    "internal: resolved behavior '{}' has no MIR slot",
                                    key.qualified_name()
                                ),
                                None => format!(
                                    "cannot resolve behavior '{}' unambiguously from dynamic receiver",
                                    behavior
                                ),
                            },
                            Span::default(),
                        )
                    })?;
                let actor_id = self.lower_operand(actor)?;'''
    text = replace_once(text, old_ask, new_ask, "MIR ask key dispatch")

    text = replace_between(
        text,
        "/// Recover the actor declaration owner from the nominal behavior schema\n",
        "fn literal_to_constant(",
        "",
        "remove MIR source-name identity helper",
    )

    path.write_text(text)


def patch_call_sites() -> None:
    pattern = re.compile(
        r"(?P<prefix>(?:(?:crate|nulang)::)?hir_lower)::lower_module\(\s*"
        r"(?P<ast>&?[A-Za-z_][A-Za-z0-9_]*)\s*,\s*&"
        r"(?P<tc>[A-Za-z_][A-Za-z0-9_]*)\.inferred_decl_types\s*\)"
    )
    total = 0
    for root in (Path("src"), Path("crates"), Path("benches"), Path("tests")):
        if not root.exists():
            continue
        for path in root.rglob("*.rs"):
            text = path.read_text()

            def repl(match: re.Match[str]) -> str:
                nonlocal total
                total += 1
                return (
                    f"{match.group('prefix')}::lower_module_with_behavior_keys("
                    f"{match.group('ast')}, &{match.group('tc')}.inferred_decl_types, "
                    f"&{match.group('tc')}.resolved_behavior_keys)"
                )

            updated = pattern.sub(repl, text)
            if updated != text:
                path.write_text(updated)
    if total == 0:
        raise SystemExit("typed HIR call sites: no lower_module calls updated")
    print(f"updated {total} typed HIR call site(s)")


def main() -> None:
    patch_semantic_schema()
    patch_typechecker()
    patch_hir()
    patch_hir_lower()
    patch_mir_lower()
    patch_call_sites()


if __name__ == "__main__":
    main()
