#!/usr/bin/env python3
"""Apply the narrow #270 compiler identity migration, idempotently.

This script exists only to make the P0 exact-head landing reproducible on a
registered GitHub Actions workflow. The landing job removes it after validation.
"""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    target = Path(path)
    text = target.read_text()
    if new in text:
        print(f"{label}: already migrated")
        return
    if old not in text:
        raise SystemExit(f"{label}: unexpected source shape")
    target.write_text(text.replace(old, new, 1))


def main() -> None:
    replace_once(
        "src/behavior_identity.rs",
        '''pub fn resolve_behavior_index_from_hint(
    actor_name_hint: &str,
    behavior: &str,
    behavior_names: &[String],
) -> Result<usize, BehaviorIdentityError> {
    if !actor_name_hint.is_empty() {
''',
        '''pub fn resolve_behavior_index_from_hint(
    actor_name_hint: &str,
    behavior: &str,
    behavior_names: &[String],
) -> Result<usize, BehaviorIdentityError> {
    // Semantic passes may already have qualified a known call as
    // `Actor.behavior`. That compiler-owned identity takes precedence over
    // the legacy lexical receiver-name hint.
    if let Some(idx) = behavior_names.iter().position(|name| name == behavior) {
        return Ok(idx);
    }

    if !actor_name_hint.is_empty() {
''',
        "qualified behavior fast path",
    )

    replace_once(
        "src/actor_protocol.rs",
        '''        Expr::Send {
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
''',
        '''        Expr::Send {
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
            let actor_name = actor_name_for_expr(actor, env);
            let short_behavior = if let Some(actor_name) = actor_name.as_deref() {
                let prefix = format!("{actor_name}.");
                behavior
                    .strip_prefix(&prefix)
                    .unwrap_or(behavior.as_str())
                    .to_string()
            } else {
                behavior.clone()
            };
            let _ = validate_call(actor, &short_behavior, args, env, protocols, *span)?;
            if let Some(actor_name) = actor_name {
                *behavior = format!("{actor_name}.{short_behavior}");
            }
            Ok(())
        }
''',
        "send qualification",
    )

    replace_once(
        "src/actor_protocol.rs",
        '''        Expr::Ask {
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
''',
        '''        Expr::Ask {
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
            let actor_name = actor_name_for_expr(actor, env);
            let short_behavior = if let Some(actor_name) = actor_name.as_deref() {
                let prefix = format!("{actor_name}.");
                behavior
                    .strip_prefix(&prefix)
                    .unwrap_or(behavior.as_str())
                    .to_string()
            } else {
                behavior.clone()
            };
            let ask_span = *span;
            let ret = validate_call(actor, &short_behavior, args, env, protocols, ask_span)?;
            if let Some(actor_name) = actor_name {
                *behavior = format!("{actor_name}.{short_behavior}");
            }
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
''',
        "ask qualification",
    )

    replace_once(
        "src/hir_lower.rs",
        '''pub fn lower_module(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
) -> hir::Module {
    let mut module = hir::Module::new(&ast.name);
''',
        '''pub fn lower_module(
    ast: &ast::AstModule,
    inferred_decl_types: &FxHashMap<String, Type>,
) -> hir::Module {
    let annotated_ast = crate::actor_protocol::annotate_module(ast)
        .expect("HIR lowering requires an actor-protocol-valid AST");
    let ast = &annotated_ast;
    let mut module = hir::Module::new(&ast.name);
''',
        "HIR protocol enrichment",
    )

    replace_once(
        "src/mir_lower.rs",
        '''    /// Resolve `send`/`ask actor behavior(...)` to a behavior-table index by
    /// name. Mirrors the stable compiler's `behavior_table_index`: an exact
    /// "ActorName.behavior" match first, falling back to any behavior with a
    /// matching suffix if the receiver expression isn't a bare actor-typed
    /// variable name (a known ambiguity inherited from the stable compiler,
    /// not introduced here).
    fn send_behavior_idx(&self, actor_name_hint: &str, behavior: &str) -> usize {
        let full_name = format!("{}.{}", actor_name_hint, behavior);
        if let Some(idx) = self.behavior_names.iter().position(|n| *n == full_name) {
            return idx;
        }
        let suffix = format!(".{}", behavior);
        self.behavior_names
            .iter()
            .position(|n| n.ends_with(&suffix))
            .unwrap_or(self.behaviors.len())
    }
''',
        '''    /// Resolve `send`/`ask actor behavior(...)` without ever choosing an
    /// arbitrary first suffix match. Known calls arrive from HIR with a
    /// compiler-qualified `Actor.behavior` identity. Dynamic calls retain
    /// compatibility only when the short behavior name is globally unique.
    fn send_behavior_idx(&self, actor_name_hint: &str, behavior: &str) -> NuResult<usize> {
        crate::behavior_identity::resolve_behavior_index_from_hint(
            actor_name_hint,
            behavior,
            &self.behavior_names,
        )
        .map_err(|error| compile_err(error.to_string(), Span::default()))
    }
''',
        "MIR behavior resolver",
    )

    mir = Path("src/mir_lower.rs")
    text = mir.read_text()
    old_call = "                let idx = self.ctx.send_behavior_idx(&actor_hint, behavior);\n"
    new_call = "                let idx = self.ctx.send_behavior_idx(&actor_hint, behavior)?;\n"
    old_count = text.count(old_call)
    new_count = text.count(new_call)
    if old_count == 2:
        mir.write_text(text.replace(old_call, new_call))
    elif old_count == 0 and new_count == 2:
        print("MIR send/ask call sites: already migrated")
    else:
        raise SystemExit(
            f"MIR call sites: unexpected shape old={old_count} new={new_count}"
        )


if __name__ == "__main__":
    main()
