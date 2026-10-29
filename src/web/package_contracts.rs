//! Package-wide web contract extraction.
//!
//! The low-level contract compiler operates on one `AstModule`. Real web
//! packages are normally split across files and commonly register routes via
//! the public `route(...)` / `route_method(...)` stdlib helpers. This pass
//! assembles package source into one contract-analysis module and lowers those
//! helper calls to the same `Web.route` representation used by direct effect
//! registrations.
//!
//! This remains an analysis-only pass: it does not mutate the program compiled
//! for execution.

use crate::ast::{AstModule, Decl, Expr, Literal, WorkflowItem};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::types::Span;
use crate::web::contracts::{compile_module_contracts, ContractCompilation};
use std::path::Path;

/// Compile web contracts from every `.nula` file below `src_root` using one
/// package-wide symbol table.
pub fn compile_contracts_from_tree(src_root: &Path) -> ContractCompilation {
    let mut modules = Vec::new();
    let mut diagnostics = Vec::new();
    collect_modules_recursive(src_root, &mut modules, &mut diagnostics);

    let mut compiled = compile_modules_contracts(&modules);
    diagnostics.append(&mut compiled.diagnostics);
    compiled.diagnostics = diagnostics;
    compiled
}

/// Compile contracts from already parsed package modules.
///
/// Keeping this separate from filesystem discovery makes the package-level
/// semantics testable and gives a future resolved-AST pipeline a direct entry
/// point without reparsing source files.
pub fn compile_modules_contracts(modules: &[AstModule]) -> ContractCompilation {
    // Helper recognition is deliberately scoped to the source module that
    // imports stdlib::web. A package may contain an unrelated local `route()`
    // function in another file, and a web import elsewhere must not turn that
    // call into framework metadata.
    let mut helpers = Vec::new();
    let mut helper_diagnostics = Vec::new();
    for module in modules {
        if imports_web_route_helpers(module) {
            collect_helper_routes_in_decls(&module.decls, &mut helpers, &mut helper_diagnostics);
        }
    }

    let mut combined = AstModule {
        name: "__web_contract_package".to_string(),
        decls: modules
            .iter()
            .flat_map(|module| module.decls.iter().cloned())
            .collect(),
    };

    for (index, helper) in helpers.into_iter().enumerate() {
        combined
            .decls
            .push(synthetic_route_registration(index, helper));
    }

    let mut compiled = compile_module_contracts(&combined);
    compiled.diagnostics.extend(helper_diagnostics);
    compiled
        .routes
        .sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
    compiled
        .routes
        .dedup_by(|a, b| a.method == b.method && a.path == b.path);
    compiled
}

fn collect_modules_recursive(
    path: &Path,
    modules: &mut Vec<AstModule>,
    diagnostics: &mut Vec<String>,
) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            diagnostics.push(format!(
                "{}: failed to read source directory",
                path.display()
            ));
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            collect_modules_recursive(&entry.path(), modules, diagnostics);
        }
        return;
    }

    if path.extension().and_then(|ext| ext.to_str()) != Some("nula") {
        return;
    }

    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            diagnostics.push(format!(
                "{}: failed to read source: {error}",
                path.display()
            ));
            return;
        }
    };
    let tokens = match Lexer::new(&source).lex() {
        Ok(tokens) => tokens,
        Err(error) => {
            diagnostics.push(format!("{}: lexer error: {error}", path.display()));
            return;
        }
    };
    match Parser::new(tokens).parse_module() {
        Ok(module) => modules.push(module),
        Err(error) => diagnostics.push(format!("{}: parser error: {error}", path.display())),
    }
}

/// Whether a source module imports the public stdlib web helper surface.
///
/// We gate bare `route(...)` recognition on this import to avoid treating an
/// unrelated user function named `route` as framework registration.
fn imports_web_route_helpers(module: &AstModule) -> bool {
    imports_web_route_helpers_in_decls(&module.decls)
}

fn imports_web_route_helpers_in_decls(decls: &[Decl]) -> bool {
    decls.iter().any(|decl| match decl {
        Decl::Import { path, .. } => {
            path == "stdlib::web"
                || path == "stdlib::web::host"
                || path.starts_with("stdlib::web::")
        }
        Decl::Module { decls, .. } => imports_web_route_helpers_in_decls(decls),
        _ => false,
    })
}

#[derive(Debug, Clone)]
struct HelperRoute {
    method: String,
    path: String,
    handler: Expr,
    response_operation: String,
    prefix: Option<String>,
}

fn synthetic_route_registration(index: usize, helper: HelperRoute) -> Decl {
    let span = Span::default();
    let mut args = vec![Expr::Literal(Literal::String(helper.method), span)];
    if let Some(prefix) = helper.prefix {
        args.push(Expr::Literal(Literal::String(prefix), span));
    }
    args.push(Expr::Literal(Literal::String(helper.path), span));
    args.push(helper.handler);
    Decl::LetBinding {
        name: format!("__web_contract_route_{index}"),
        type_ann: None,
        value: Expr::Perform {
            effect: "Web".to_string(),
            op: helper.response_operation,
            args,
            span,
        },
        mutable: false,
        span,
    }
}

fn collect_helper_routes_in_decls(
    decls: &[Decl],
    out: &mut Vec<HelperRoute>,
    diagnostics: &mut Vec<String>,
) {
    for decl in decls {
        match decl {
            Decl::Function { body, .. }
            | Decl::LetBinding { value: body, .. }
            | Decl::Signal { init: body, .. }
            | Decl::Given { value: body, .. } => {
                collect_helper_routes_in_expr(body, out, diagnostics)
            }
            Decl::Workflow { items, .. } => {
                for item in items {
                    match item {
                        WorkflowItem::Step(step) => {
                            collect_helper_routes_in_expr(&step.body, out, diagnostics)
                        }
                        WorkflowItem::Parallel(steps) => {
                            for step in steps {
                                collect_helper_routes_in_expr(&step.body, out, diagnostics);
                            }
                        }
                    }
                }
            }
            Decl::Module { decls, .. } => collect_helper_routes_in_decls(decls, out, diagnostics),
            _ => {}
        }
    }
}

fn collect_helper_routes_in_expr(
    expr: &Expr,
    out: &mut Vec<HelperRoute>,
    diagnostics: &mut Vec<String>,
) {
    if let Expr::App { func, args, .. } = expr {
        if let Expr::Var(name, _) = func.as_ref() {
            match (name.as_str(), args.as_slice()) {
                ("route", [path, handler]) => {
                    if let Some(path) = string_literal(path) {
                        out.push(HelperRoute {
                            method: "GET".to_string(),
                            path,
                            handler: handler.clone(),
                            response_operation: "route".into(),
                            prefix: None,
                        });
                    }
                }
                ("route_method" | "route_json" | "route_json_typed", [method, path, handler]) => {
                    if let (Some(method), Some(path)) =
                        (string_literal(method), string_literal(path))
                    {
                        out.push(HelperRoute {
                            method,
                            path,
                            handler: handler.clone(),
                            response_operation: if name == "route_method" {
                                "route".into()
                            } else {
                                name.clone()
                            },
                            prefix: None,
                        });
                    }
                }
                (
                    "route_scoped" | "route_json_scoped" | "route_json_typed_scoped",
                    [method, prefix, path, handler],
                ) => {
                    if let (Some(method), Some(prefix), Some(path)) = (
                        string_literal(method),
                        string_literal(prefix),
                        string_literal(path),
                    ) {
                        out.push(HelperRoute {
                            method,
                            prefix: Some(prefix),
                            path,
                            handler: handler.clone(),
                            response_operation: name.clone(),
                        });
                    } else {
                        diagnostics.push(format!(
                            "{name} requires literal method, prefix, and path for scoped route extraction"
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    match expr {
        Expr::Lambda { body, .. } => collect_helper_routes_in_expr(body, out, diagnostics),
        Expr::App { func, args, .. } => {
            collect_helper_routes_in_expr(func, out, diagnostics);
            for arg in args {
                collect_helper_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_helper_routes_in_expr(value, out, diagnostics);
            collect_helper_routes_in_expr(body, out, diagnostics);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_helper_routes_in_expr(cond, out, diagnostics);
            collect_helper_routes_in_expr(then_branch, out, diagnostics);
            if let Some(else_branch) = else_branch {
                collect_helper_routes_in_expr(else_branch, out, diagnostics);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_helper_routes_in_expr(scrutinee, out, diagnostics);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_helper_routes_in_expr(guard, out, diagnostics);
                }
                collect_helper_routes_in_expr(body, out, diagnostics);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for expr in exprs {
                collect_helper_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) | Expr::FString(exprs, ..) => {
            for expr in exprs {
                collect_helper_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, expr) in fields {
                collect_helper_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::FieldAccess { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. } => collect_helper_routes_in_expr(expr, out, diagnostics),
        Expr::Index { arr, idx, .. } => {
            collect_helper_routes_in_expr(arr, out, diagnostics);
            collect_helper_routes_in_expr(idx, out, diagnostics);
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            collect_helper_routes_in_expr(left, out, diagnostics);
            collect_helper_routes_in_expr(right, out, diagnostics);
        }
        Expr::Assign { target, value, .. } => {
            collect_helper_routes_in_expr(target, out, diagnostics);
            collect_helper_routes_in_expr(value, out, diagnostics);
        }
        Expr::For { iterable, body, .. } => {
            collect_helper_routes_in_expr(iterable, out, diagnostics);
            collect_helper_routes_in_expr(body, out, diagnostics);
        }
        Expr::While { cond, body, .. } => {
            collect_helper_routes_in_expr(cond, out, diagnostics);
            collect_helper_routes_in_expr(body, out, diagnostics);
        }
        Expr::Return(value, ..) | Expr::Break(value, ..) => {
            if let Some(value) = value {
                collect_helper_routes_in_expr(value, out, diagnostics);
            }
        }
        Expr::Recover { body, .. } | Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
            collect_helper_routes_in_expr(body, out, diagnostics)
        }
        Expr::Defer { expr, .. } => collect_helper_routes_in_expr(expr, out, diagnostics),
        Expr::Handle { body, handlers, .. } => {
            collect_helper_routes_in_expr(body, out, diagnostics);
            for handler in handlers {
                collect_helper_routes_in_expr(&handler.body, out, diagnostics);
            }
        }
        Expr::Perform { args, .. } | Expr::Emit { args, .. } => {
            for arg in args {
                collect_helper_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_helper_routes_in_expr(actor_type, out, diagnostics);
            for (_, expr) in init {
                collect_helper_routes_in_expr(expr, out, diagnostics);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_helper_routes_in_expr(arg, out, diagnostics);
                }
            }
            if let Some(target) = target_node {
                collect_helper_routes_in_expr(target, out, diagnostics);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_helper_routes_in_expr(actor, out, diagnostics);
            for arg in args {
                collect_helper_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_helper_routes_in_expr(guard, out, diagnostics);
                }
                collect_helper_routes_in_expr(body, out, diagnostics);
            }
            if let Some((timeout, body)) = after {
                collect_helper_routes_in_expr(timeout, out, diagnostics);
                collect_helper_routes_in_expr(body, out, diagnostics);
            }
        }
        Expr::GrainRef { key, .. } => collect_helper_routes_in_expr(key, out, diagnostics),
        Expr::Resume { value, .. } => collect_helper_routes_in_expr(value, out, diagnostics),
        Expr::Migrate { actor, node, .. } => {
            collect_helper_routes_in_expr(actor, out, diagnostics);
            collect_helper_routes_in_expr(node, out, diagnostics);
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
    fn resolves_handler_metadata_across_source_modules() {
        let handlers = parse(
            r#"
type UserId = String
fn show_user(id: UserId) -> String ! {Request} { "ok" }
"#,
        );
        let routes = parse(
            r#"
fn web_main() {
    perform Web.route("GET", "/users/{id: UserId}", show_user)
}
"#,
        );

        let compiled = compile_modules_contracts(&[handlers, routes]);
        assert!(compiled.diagnostics.is_empty());
        assert_eq!(compiled.routes.len(), 1);
        let route = &compiled.routes[0];
        assert_eq!(route.handler.as_deref(), Some("show_user"));
        assert_eq!(route.handler_params[0].name, "id");
        assert_eq!(route.response_type.as_deref(), Some("String"));
        assert!(route.effects.contains(&"Request".to_string()));
    }

    #[test]
    fn scoped_helper_routes_share_compiler_checked_prefix_semantics() {
        let ast = parse(
            r#"
import stdlib::web
fn show() -> String { "ok" }
fn register() {
    route_scoped("GET", "/api/v1", "/health", show)
}
"#,
        );
        let compiled = compile_modules_contracts(&[ast]);
        assert!(
            compiled.diagnostics.is_empty(),
            "{:?}",
            compiled.diagnostics
        );
        assert_eq!(compiled.routes.len(), 1);
        assert_eq!(compiled.routes[0].path, "/api/v1/health");
    }

    #[test]
    fn dynamic_scoped_helper_is_explicit_diagnostic_not_omitted_route() {
        let ast = parse(
            r#"
import stdlib::web
fn show() -> String { "ok" }
fn register(prefix: String) {
    route_scoped("GET", prefix, "/health", show)
}
"#,
        );
        let compiled = compile_modules_contracts(&[ast]);
        assert!(compiled.routes.is_empty());
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|d| d.contains("route_scoped requires literal method, prefix, and path")),
            "{:?}",
            compiled.diagnostics
        );
    }

    #[test]
    fn recognizes_public_route_helper_calls() {
        let module = parse(
            r#"
import stdlib::web::host

type UserId = String
fn show_user(id: UserId) -> String { "ok" }

fn web_main() {
    route("/users/{id: UserId}", show_user)
    route_method("POST", "/users/{id: UserId}", show_user)
}
"#,
        );

        let compiled = compile_modules_contracts(&[module]);
        assert!(compiled.diagnostics.is_empty());
        assert_eq!(compiled.routes.len(), 2);
        assert!(compiled
            .routes
            .iter()
            .any(|route| route.method == "GET" && route.handler.as_deref() == Some("show_user")));
        assert!(compiled
            .routes
            .iter()
            .any(|route| route.method == "POST" && route.handler.as_deref() == Some("show_user")));
    }

    #[test]
    fn does_not_treat_unimported_user_route_function_as_framework_helper() {
        let module = parse(
            r#"
fn route(path, callback) { nil }
fn callback() { nil }
fn main() { route("/not-web", callback) }
"#,
        );

        let compiled = compile_modules_contracts(&[module]);
        assert!(compiled.routes.is_empty());
    }

    #[test]
    fn web_import_in_one_file_does_not_capture_route_call_in_another() {
        let web_module = parse(
            r#"
import stdlib::web::host
fn home() { "ok" }
fn web_main() { route("/", home) }
"#,
        );
        let unrelated = parse(
            r#"
fn route(path, callback) { nil }
fn local_handler() { nil }
fn utility() { route("/not-a-web-route", local_handler) }
"#,
        );

        let compiled = compile_modules_contracts(&[web_module, unrelated]);
        assert_eq!(compiled.routes.len(), 1);
        assert_eq!(compiled.routes[0].path, "/");
        assert_eq!(compiled.routes[0].handler.as_deref(), Some("home"));
    }
}
