//! RFC 0008 migration-purity validation.
//!
//! Migration bodies are replay code, not ordinary effect-handled application
//! code. Their contract is deliberately stricter than an inferred empty effect
//! row: `perform` remains forbidden even when locally handled, external calls
//! are forbidden, and direct helper calls are expanded interprocedurally so an
//! effect cannot be hidden behind a pure-looking wrapper.

use std::collections::{HashMap, HashSet};

use crate::ast::{Decl, Expr, Pattern};
use crate::types::{NuError, NuResult, Span};

struct FunctionInfo<'a> {
    body: &'a Expr,
    params: Vec<String>,
}

struct MigrationPurityChecker<'a> {
    functions: HashMap<String, FunctionInfo<'a>>,
    externs: HashSet<String>,
    visiting: Vec<String>,
}

impl<'a> MigrationPurityChecker<'a> {
    fn new(decls: &'a [Decl]) -> Self {
        let mut checker = Self {
            functions: HashMap::new(),
            externs: HashSet::new(),
            visiting: Vec::new(),
        };
        checker.collect(decls);
        checker
    }

    fn collect(&mut self, decls: &'a [Decl]) {
        for decl in decls {
            match decl {
                Decl::Module { decls, .. } => self.collect(decls),
                Decl::Function {
                    name, params, body, ..
                } => {
                    self.functions.insert(
                        name.clone(),
                        FunctionInfo {
                            body,
                            params: params.iter().map(|p| p.name.clone()).collect(),
                        },
                    );
                }
                Decl::Extern { funcs, .. } => {
                    self.externs.extend(funcs.iter().map(|f| f.name.clone()));
                }
                _ => {}
            }
        }
    }

    fn check_decls(&mut self, decls: &'a [Decl]) -> NuResult<()> {
        for decl in decls {
            match decl {
                Decl::Module { decls, .. } => self.check_decls(decls)?,
                Decl::Actor {
                    name, migrations, ..
                } => {
                    for migration in migrations {
                        let scope = format!(
                            "migration {} -> {} of entity '{}'",
                            migration.from_version, migration.to_version, name
                        );
                        if let Some(body) = &migration.state_body {
                            self.walk(&scope, body, &[])?;
                        }
                        for (_event, params, body) in &migration.event_migrations {
                            self.walk(&scope, body, params)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn forbidden(&self, scope: &str, operation: &str, span: Span) -> NuError {
        NuError::effect_error(
            format!(
                "{scope}: '{operation}' is forbidden in RFC 0008 migrations; migrations must be deterministic functions of old state/events and may only use pure expressions plus replay-stream `emit`"
            ),
            span,
        )
    }

    fn expand_function(
        &mut self,
        scope: &str,
        name: &str,
    ) -> NuResult<()> {
        if self.visiting.iter().any(|n| n == name) {
            return Ok(());
        }
        let Some(info) = self.functions.get(name) else {
            return Ok(());
        };
        let body = info.body;
        let params = info.params.clone();
        self.visiting.push(name.to_string());
        let result = self.walk(scope, body, &params);
        self.visiting.pop();
        result
    }

    fn walk(&mut self, scope: &str, expr: &'a Expr, bound: &[String]) -> NuResult<()> {
        match expr {
            Expr::Perform {
                effect, op, span, ..
            } => Err(self.forbidden(scope, &format!("perform {effect}.{op}"), *span)),
            Expr::Spawn { span, .. } => Err(self.forbidden(scope, "spawn", *span)),
            Expr::Send { span, .. } => Err(self.forbidden(scope, "send", *span)),
            Expr::Ask { span, .. } => Err(self.forbidden(scope, "ask", *span)),
            Expr::Receive { span, .. } => Err(self.forbidden(scope, "receive/after", *span)),
            Expr::Migrate { span, .. } => Err(self.forbidden(scope, "actor migrate", *span)),
            Expr::GrainRef { span, .. } => Err(self.forbidden(
                scope,
                "virtual-entity lookup",
                *span,
            )),
            Expr::Resume { span, .. } => Err(self.forbidden(scope, "resume", *span)),
            Expr::Defer { span, .. } => Err(self.forbidden(scope, "defer/errdefer", *span)),

            Expr::App { func, args, span } => {
                self.walk(scope, func, bound)?;
                for arg in args {
                    self.walk(scope, arg, bound)?;
                }
                if let Expr::Var(name, _) = func.as_ref() {
                    if !bound.iter().any(|b| b == name) {
                        if self.externs.contains(name) {
                            return Err(self.forbidden(
                                scope,
                                &format!("external/FFI call '{name}'"),
                                *span,
                            ));
                        }
                        self.expand_function(scope, name)?;
                    }
                }
                Ok(())
            }

            Expr::Lambda { params, body, .. } => {
                let mut inner = bound.to_vec();
                inner.extend(params.iter().map(|p| p.name.clone()));
                self.walk(scope, body, &inner)
            }
            Expr::Let {
                name, value, body, ..
            } => {
                self.walk(scope, value, bound)?;
                let mut inner = bound.to_vec();
                inner.push(name.clone());
                self.walk(scope, body, &inner)
            }
            Expr::LetRec {
                name,
                params,
                value,
                body,
                ..
            } => {
                let mut value_bound = bound.to_vec();
                value_bound.push(name.clone());
                value_bound.extend(params.iter().map(|p| p.name.clone()));
                self.walk(scope, value, &value_bound)?;
                let mut body_bound = bound.to_vec();
                body_bound.push(name.clone());
                self.walk(scope, body, &body_bound)
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                self.walk(scope, cond, bound)?;
                self.walk(scope, then_branch, bound)?;
                if let Some(other) = else_branch {
                    self.walk(scope, other, bound)?;
                }
                Ok(())
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.walk(scope, scrutinee, bound)?;
                for (pattern, guard, body) in arms {
                    let mut inner = bound.to_vec();
                    pattern_bindings(pattern, &mut inner);
                    if let Some(guard) = guard {
                        self.walk(scope, guard, &inner)?;
                    }
                    self.walk(scope, body, &inner)?;
                }
                Ok(())
            }
            Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
                for expr in exprs {
                    self.walk(scope, expr, bound)?;
                }
                Ok(())
            }
            Expr::FString(parts, _) | Expr::Tuple(parts, _) | Expr::Array(parts, _) => {
                for expr in parts {
                    self.walk(scope, expr, bound)?;
                }
                Ok(())
            }
            Expr::Record(fields, _) => {
                for (_, expr) in fields {
                    self.walk(scope, expr, bound)?;
                }
                Ok(())
            }
            Expr::RecordUpdate { base, fields, .. } => {
                self.walk(scope, base, bound)?;
                for (_, expr) in fields {
                    self.walk(scope, expr, bound)?;
                }
                Ok(())
            }
            Expr::FieldAccess { expr, .. }
            | Expr::Unary { expr, .. }
            | Expr::CapAnnotate { expr, .. }
            | Expr::TypeAnnotate { expr, .. }
            | Expr::Consume { expr, .. }
            | Expr::Recover { body: expr, .. } => self.walk(scope, expr, bound),
            Expr::Index { arr, idx, .. } => {
                self.walk(scope, arr, bound)?;
                self.walk(scope, idx, bound)
            }
            Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
                self.walk(scope, left, bound)?;
                self.walk(scope, right, bound)
            }
            Expr::Assign { target, value, .. } => {
                self.walk(scope, target, bound)?;
                self.walk(scope, value, bound)
            }
            Expr::Emit { args, .. } => {
                // RFC 0008 explicitly permits replay-stream emit. Its arguments
                // must still be pure.
                for arg in args {
                    self.walk(scope, arg, bound)?;
                }
                Ok(())
            }
            Expr::Handle { body, handlers, .. } => {
                // Handling does not launder a forbidden `perform`: recurse into
                // both the handled body and handler arms without exemptions.
                self.walk(scope, body, bound)?;
                for handler in handlers {
                    let mut inner = bound.to_vec();
                    inner.extend(handler.params.iter().cloned());
                    self.walk(scope, &handler.body, &inner)?;
                }
                Ok(())
            }
            Expr::For {
                var,
                iterable,
                body,
                ..
            } => {
                self.walk(scope, iterable, bound)?;
                let mut inner = bound.to_vec();
                inner.push(var.clone());
                self.walk(scope, body, &inner)
            }
            Expr::While { cond, body, .. } => {
                self.walk(scope, cond, bound)?;
                self.walk(scope, body, bound)
            }
            Expr::Return(Some(expr), _) | Expr::Break(Some(expr), _) => {
                self.walk(scope, expr, bound)
            }
            Expr::Literal(..)
            | Expr::Var(..)
            | Expr::SelfRef(..)
            | Expr::Return(None, _)
            | Expr::Break(None, _)
            | Expr::Panic(..) => Ok(()),
        }
    }
}

fn pattern_bindings(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        Pattern::Wild | Pattern::Lit(_) => {}
        Pattern::Var(name) => out.push(name.clone()),
        Pattern::Alias(name, inner) => {
            out.push(name.clone());
            pattern_bindings(inner, out);
        }
        Pattern::Tuple(items) => {
            for item in items {
                pattern_bindings(item, out);
            }
        }
        Pattern::Record(fields) => {
            for (_, item) in fields {
                pattern_bindings(item, out);
            }
        }
        Pattern::Variant(_, Some(inner)) => pattern_bindings(inner, out),
        Pattern::Variant(_, None) => {}
    }
}

/// Enforce RFC 0008 purity for every migration in a module, including nested
/// modules and direct calls through module-level helper functions.
pub fn check_module(decls: &[Decl]) -> NuResult<()> {
    let mut checker = MigrationPurityChecker::new(decls);
    checker.check_decls(decls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> crate::ast::AstModule {
        let tokens = Lexer::new(source).lex().expect("lex");
        Parser::new(tokens).parse_module().expect("parse")
    }

    #[test]
    fn rejects_direct_perform() {
        let ast = parse(
            r#"
            entity Account {
                version: 2
                state balance: Int = 0
                events | Changed(value: Int)
                migration from 1 to 2 {
                    state => { perform IO.print("migration") }
                }
            }
            "#,
        );
        let err = check_module(&ast.decls).unwrap_err().to_string();
        assert!(err.contains("migration 1 -> 2"));
        assert!(err.contains("perform IO.print"));
    }

    #[test]
    fn rejects_effect_hidden_in_helper() {
        let ast = parse(
            r#"
            fn noisy() { perform IO.print("x") }
            entity Account {
                version: 2
                state balance: Int = 0
                events | Changed(value: Int)
                migration from 1 to 2 {
                    state => { noisy() }
                }
            }
            "#,
        );
        assert!(check_module(&ast.decls).is_err());
    }

    #[test]
    fn permits_replay_stream_emit() {
        let ast = parse(
            r#"
            entity Account {
                version: 2
                state balance: Int = 0
                events | Changed(value: Int)
                migration from 1 to 2 {
                    events {
                        | Changed(value) => emit Changed(value)
                        | other => other
                    }
                }
            }
            "#,
        );
        assert!(check_module(&ast.decls).is_ok());
    }

    #[test]
    fn public_effect_checker_runs_migration_gate() {
        let ast = parse(
            r#"
            entity Account {
                version: 2
                state balance: Int = 0
                events | Changed(value: Int)
                migration from 1 to 2 {
                    state => { perform IO.print("x") }
                }
            }
            "#,
        );
        let mut checker = crate::effect_checker::EffectChecker::new();
        assert!(checker.check_module(&ast.decls).is_err());
    }
}
