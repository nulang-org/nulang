//! Compile-time web route contracts.
//!
//! This module bridges Nulang's existing `perform Web.route(...)` surface to a
//! structured contract representation.  The deployment IR can consume these
//! contracts without relying on string scans for handler metadata.
//!
//! The first version is deliberately backwards compatible with `:name` path
//! parameters.  It also understands `{name}` and `{name: Type}` segments so the
//! IR format is ready for the typed route syntax without forcing a runtime
//! migration in the same change.

use crate::ast::{AstModule, Decl, Expr, FunctionAnnotation, Literal, Param, WorkflowItem};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::types::EffectRow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteParamContract {
    pub name: String,
    /// Source-level type name when known.  Legacy `:name` segments inherit the
    /// type from a same-named handler parameter when one is declared.
    pub ty: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerParamContract {
    pub name: String,
    pub ty: Option<String>,
    pub capability: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteContract {
    pub method: String,
    pub path: String,
    pub handler: Option<String>,
    pub params: Vec<RouteParamContract>,
    pub handler_params: Vec<HandlerParamContract>,
    pub response_type: Option<String>,
    pub error_type: Option<String>,
    pub effects: Vec<String>,
    /// Nulang reference capability (`iso`, `ref`, `val`, ...), when the handler
    /// declares one.  Resource/security capabilities will be represented by a
    /// separate field once capability-parameterized effects land.
    pub reference_capability: Option<String>,
    pub placement: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractCompilation {
    pub routes: Vec<RouteContract>,
    /// Best-effort source discovery must not make deployment IR generation
    /// panic.  Parse failures are retained here so build tooling can surface
    /// them and can become hard errors once the contract pass is part of the
    /// typed compiler pipeline.
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
struct FunctionMeta {
    params: Vec<Param>,
    response_type: Option<String>,
    error_type: Option<String>,
    effects: Vec<String>,
    reference_capability: Option<String>,
    placement: Option<String>,
}

impl FunctionMeta {
    fn from_decl(decl: &Decl) -> Option<(String, Self)> {
        let Decl::Function {
            name,
            params,
            ret_type,
            error_type,
            effect,
            cap,
            annotations,
            ..
        } = decl
        else {
            return None;
        };

        let placement = annotations.iter().find_map(|annotation| match annotation {
            FunctionAnnotation::Placement(p) => Some(p.to_string()),
            _ => None,
        });

        Some((
            name.clone(),
            Self {
                params: params.clone(),
                response_type: ret_type.as_ref().map(ToString::to_string),
                error_type: error_type.as_ref().map(ToString::to_string),
                effects: effect_names(effect.as_ref()),
                reference_capability: cap.map(|c| c.to_string()),
                placement,
            },
        ))
    }
}

/// Compile contracts from every `.nula` source file below `src_root`.
///
/// Parsing is intentionally file-local and best effort.  Package builds have
/// already validated their source through the normal compiler pipeline; this
/// pass exists to preserve static web metadata in the deployment artifact.
pub fn compile_contracts_from_tree(src_root: &Path) -> ContractCompilation {
    let mut out = ContractCompilation::default();
    collect_contracts_recursive(src_root, &mut out);
    out.routes
        .sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
    out.routes
        .dedup_by(|a, b| a.method == b.method && a.path == b.path);
    out
}

fn collect_contracts_recursive(path: &Path, out: &mut ContractCompilation) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            collect_contracts_recursive(&entry.path(), out);
        }
        return;
    }

    if path.extension().and_then(|e| e.to_str()) != Some("nula") {
        return;
    }

    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: failed to read source: {err}", path.display()));
            return;
        }
    };

    let tokens = match Lexer::new(&source).lex() {
        Ok(tokens) => tokens,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: lexer error: {err}", path.display()));
            return;
        }
    };
    let module = match Parser::new(tokens).parse_module() {
        Ok(module) => module,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: parser error: {err}", path.display()));
            return;
        }
    };

    out.routes.extend(contracts_from_module(&module));
}

/// Extract static route contracts from a parsed module.
pub fn contracts_from_module(module: &AstModule) -> Vec<RouteContract> {
    let functions: HashMap<String, FunctionMeta> = module
        .decls
        .iter()
        .filter_map(FunctionMeta::from_decl)
        .collect();

    let mut raw_routes = Vec::new();
    collect_routes_in_module(module, &mut raw_routes);

    raw_routes
        .into_iter()
        .map(|(method, path, handler)| {
            let handler_name = match &handler {
                Expr::Var(name, _) => Some(name.clone()),
                _ => None,
            };
            let meta = handler_name
                .as_ref()
                .and_then(|name| functions.get(name.as_str()));

            let mut params = parse_path_params(&path).unwrap_or_default();
            if let Some(meta) = meta {
                for route_param in &mut params {
                    if route_param.ty.is_none() {
                        route_param.ty = meta
                            .params
                            .iter()
                            .find(|param| param.name == route_param.name)
                            .and_then(|param| param.ty.as_ref())
                            .map(ToString::to_string);
                    }
                }
            }

            RouteContract {
                method,
                path,
                handler: handler_name,
                params,
                handler_params: meta
                    .map(|m| m.params.iter().map(handler_param_contract).collect())
                    .unwrap_or_default(),
                response_type: meta.and_then(|m| m.response_type.clone()),
                error_type: meta.and_then(|m| m.error_type.clone()),
                effects: meta.map(|m| m.effects.clone()).unwrap_or_default(),
                reference_capability: meta.and_then(|m| m.reference_capability.clone()),
                placement: meta.and_then(|m| m.placement.clone()),
            }
        })
        .collect()
}

fn handler_param_contract(param: &Param) -> HandlerParamContract {
    HandlerParamContract {
        name: param.name.clone(),
        ty: param.ty.as_ref().map(ToString::to_string),
        capability: param.cap.map(|cap| cap.to_string()),
    }
}

fn effect_names(row: Option<&EffectRow>) -> Vec<String> {
    let effects = match row {
        Some(EffectRow::Closed(effects)) | Some(EffectRow::Open(effects, _)) => effects,
        None => return Vec::new(),
    };
    effects.iter().map(ToString::to_string).collect()
}

/// Parse route parameters from legacy and contract-first path syntax.
///
/// Supported forms:
/// - `/users/:id`
/// - `/users/{id}`
/// - `/users/{id: UserId}`
pub fn parse_path_params(path: &str) -> Result<Vec<RouteParamContract>, String> {
    let mut params = Vec::new();
    for segment in path.split('/') {
        if let Some(name) = segment.strip_prefix(':') {
            if name.is_empty() {
                return Err(format!("route '{path}' contains an empty parameter"));
            }
            params.push(RouteParamContract {
                name: name.to_string(),
                ty: None,
            });
            continue;
        }

        if segment.starts_with('{') || segment.ends_with('}') {
            if !(segment.starts_with('{') && segment.ends_with('}')) {
                return Err(format!("route '{path}' contains malformed parameter '{segment}'"));
            }
            let inner = &segment[1..segment.len() - 1];
            let (name, ty) = match inner.split_once(':') {
                Some((name, ty)) => (name.trim(), Some(ty.trim())),
                None => (inner.trim(), None),
            };
            if name.is_empty() {
                return Err(format!("route '{path}' contains an empty parameter"));
            }
            if matches!(ty, Some("")) {
                return Err(format!("route '{path}' parameter '{name}' has an empty type"));
            }
            params.push(RouteParamContract {
                name: name.to_string(),
                ty: ty.map(str::to_string),
            });
        }
    }
    Ok(params)
}

fn collect_routes_in_module(module: &AstModule, out: &mut Vec<(String, String, Expr)>) {
    for decl in &module.decls {
        match decl {
            Decl::Function { body, .. }
            | Decl::LetBinding { value: body, .. }
            | Decl::Signal { init: body, .. } => collect_routes_in_expr(body, out),
            Decl::Workflow { items, .. } => {
                for item in items {
                    match item {
                        WorkflowItem::Step(step) => collect_routes_in_expr(&step.body, out),
                        WorkflowItem::Parallel(steps) => {
                            for step in steps {
                                collect_routes_in_expr(&step.body, out);
                            }
                        }
                    }
                }
            }
            Decl::Module { decls, .. } => {
                let nested = AstModule {
                    name: module.name.clone(),
                    decls: decls.clone(),
                    ..module.clone()
                };
                collect_routes_in_module(&nested, out);
            }
            _ => {}
        }
    }
}

fn collect_routes_in_expr(expr: &Expr, out: &mut Vec<(String, String, Expr)>) {
    if let Expr::Perform {
        effect, op, args, ..
    } = expr
    {
        if effect == "Web" && op == "route" && args.len() == 3 {
            if let (Some(method), Some(path)) = (string_literal(&args[0]), string_literal(&args[1])) {
                out.push((method, path, args[2].clone()));
            }
        }
        for arg in args {
            collect_routes_in_expr(arg, out);
        }
        return;
    }

    match expr {
        Expr::Lambda { body, .. } => collect_routes_in_expr(body, out),
        Expr::App { func, args, .. } => {
            collect_routes_in_expr(func, out);
            for arg in args {
                collect_routes_in_expr(arg, out);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_routes_in_expr(value, out);
            collect_routes_in_expr(body, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_routes_in_expr(cond, out);
            collect_routes_in_expr(then_branch, out);
            if let Some(else_branch) = else_branch {
                collect_routes_in_expr(else_branch, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_routes_in_expr(scrutinee, out);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_routes_in_expr(guard, out);
                }
                collect_routes_in_expr(body, out);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for expr in exprs {
                collect_routes_in_expr(expr, out);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) | Expr::FString(exprs, ..) => {
            for expr in exprs {
                collect_routes_in_expr(expr, out);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, expr) in fields {
                collect_routes_in_expr(expr, out);
            }
        }
        Expr::FieldAccess { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. } => collect_routes_in_expr(expr, out),
        Expr::Index { arr, idx, .. } => {
            collect_routes_in_expr(arr, out);
            collect_routes_in_expr(idx, out);
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            collect_routes_in_expr(left, out);
            collect_routes_in_expr(right, out);
        }
        Expr::Assign { target, value, .. } => {
            collect_routes_in_expr(target, out);
            collect_routes_in_expr(value, out);
        }
        Expr::For { iterable, body, .. } => {
            collect_routes_in_expr(iterable, out);
            collect_routes_in_expr(body, out);
        }
        Expr::While { cond, body, .. } => {
            collect_routes_in_expr(cond, out);
            collect_routes_in_expr(body, out);
        }
        Expr::Return(value, ..) | Expr::Break(value, ..) => {
            if let Some(value) = value {
                collect_routes_in_expr(value, out);
            }
        }
        Expr::Recover { body, .. } | Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
            collect_routes_in_expr(body, out)
        }
        Expr::Defer { expr, .. } => collect_routes_in_expr(expr, out),
        Expr::Handle { body, handlers, .. } => {
            collect_routes_in_expr(body, out);
            for handler in handlers {
                collect_routes_in_expr(&handler.body, out);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                collect_routes_in_expr(arg, out);
            }
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_routes_in_expr(actor_type, out);
            for (_, expr) in init {
                collect_routes_in_expr(expr, out);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_routes_in_expr(arg, out);
                }
            }
            if let Some(target) = target_node {
                collect_routes_in_expr(target, out);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_routes_in_expr(actor, out);
            for arg in args {
                collect_routes_in_expr(arg, out);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_routes_in_expr(guard, out);
                }
                collect_routes_in_expr(body, out);
            }
            if let Some((timeout, body)) = after {
                collect_routes_in_expr(timeout, out);
                collect_routes_in_expr(body, out);
            }
        }
        Expr::GrainRef { key, .. } => collect_routes_in_expr(key, out),
        Expr::Resume { value, .. } => collect_routes_in_expr(value, out),
        Expr::Migrate { actor, node, .. } => {
            collect_routes_in_expr(actor, out);
            collect_routes_in_expr(node, out);
        }
        _ => {}
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(value), ..) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> AstModule {
        let tokens = Lexer::new(source).lex().unwrap();
        Parser::new(tokens).parse_module().unwrap()
    }

    #[test]
    fn parses_legacy_and_typed_path_params() {
        let legacy = parse_path_params("/users/:id/posts/:post_id").unwrap();
        assert_eq!(legacy[0].name, "id");
        assert_eq!(legacy[0].ty, None);

        let typed = parse_path_params("/orgs/{org_id: OrgId}/members/{member_id}").unwrap();
        assert_eq!(typed[0].name, "org_id");
        assert_eq!(typed[0].ty.as_deref(), Some("OrgId"));
        assert_eq!(typed[1].name, "member_id");
        assert_eq!(typed[1].ty, None);
    }

    #[test]
    fn contract_inherits_legacy_param_type_from_handler() {
        let module = parse(
            r#"
type UserId = String

fn show_user(id: UserId) -> String ! {DB, Request} {
    let _ = perform Web.param("id")
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/:id", show_user)
}
"#,
        );

        let contracts = contracts_from_module(&module);
        assert_eq!(contracts.len(), 1);
        let route = &contracts[0];
        assert_eq!(route.handler.as_deref(), Some("show_user"));
        assert_eq!(route.params[0].ty.as_deref(), Some("UserId"));
        assert_eq!(route.response_type.as_deref(), Some("String"));
        assert!(route.effects.contains(&"DB".to_string()));
        assert!(route.effects.contains(&"Request".to_string()));
    }

    #[test]
    fn explicit_typed_path_wins_over_handler_fallback() {
        let module = parse(
            r#"
type UserId = String
type ExternalId = String

fn show_user(id: UserId) -> String {
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/{id: ExternalId}", show_user)
}
"#,
        );

        let contracts = contracts_from_module(&module);
        assert_eq!(contracts[0].params[0].ty.as_deref(), Some("ExternalId"));
    }
}
