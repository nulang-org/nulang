//! Compile-time route parameter checking for the Nulang web framework.
//!
//! This pass validates that route handlers read the parameters declared in the
//! route path, and that they do not read parameters that are not declared.
//! It is intentionally conservative: it only checks routes and handlers that
//! can be resolved statically.

use crate::ast::{AstModule, Decl, Expr, Literal, WorkflowItem};
use std::collections::{HashMap, HashSet};

/// A diagnostic produced by the route check pass.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCheckDiagnostic {
    pub message: String,
}

/// Check all routes in a module for parameter consistency.
///
/// For every `perform Web.route(method, path, handler)` call found in the
/// module, the pass extracts path parameters from `path` (e.g. `/users/:id`)
/// and compares them with the `perform Web.param("name")` calls in the
/// handler body. It returns a list of diagnostics for mismatches.
pub fn check_module(module: &AstModule) -> Vec<RouteCheckDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut functions: HashMap<&str, &Expr> = HashMap::new();
    for decl in &module.decls {
        if let Decl::Function { name, body, .. } = decl {
            functions.insert(name, body);
        }
    }

    let mut routes = Vec::new();
    collect_routes_in_module(module, &mut routes);

    for (method, path, handler) in &routes {
        let declared = parse_path_params(path);
        let handler_body = match handler {
            Expr::Var(name, _) => functions.get(name.as_str()).copied(),
            Expr::Lambda { body, .. } => Some(body.as_ref()),
            _ => None,
        };
        let Some(body) = handler_body else {
            continue;
        };

        let read = collect_param_reads(body);
        let declared_set: HashSet<&str> = declared.iter().map(|s| s.as_str()).collect();
        let read_set: HashSet<&str> = read.iter().map(|s| s.as_str()).collect();

        for param in &declared {
            if !read_set.contains(param.as_str()) {
                diagnostics.push(RouteCheckDiagnostic {
                    message: format!(
                        "route {} {} declares parameter ':{}' but handler never reads Web.param(\"{}\")",
                        method, path, param, param
                    ),
                });
            }
        }
        for param in &read {
            if !declared_set.contains(param.as_str()) {
                diagnostics.push(RouteCheckDiagnostic {
                    message: format!(
                        "handler reads Web.param(\"{}\") but route {} {} does not declare ':{}'",
                        param, method, path, param
                    ),
                });
            }
        }
    }

    diagnostics
}

/// Collect all `perform Web.route(...)` calls in the module.
fn collect_routes_in_module(module: &AstModule, out: &mut Vec<(String, String, Expr)>) {
    for decl in &module.decls {
        match decl {
            Decl::Function { body, .. }
            | Decl::LetBinding { value: body, .. }
            | Decl::Signal { init: body, .. } => {
                collect_routes_in_expr(body, out);
            }
            Decl::Workflow { items, .. } => {
                for item in items {
                    match item {
                        WorkflowItem::Step(s) => collect_routes_in_expr(&s.body, out),
                        WorkflowItem::Parallel(steps) => {
                            for s in steps {
                                collect_routes_in_expr(&s.body, out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_routes_in_expr(expr: &Expr, out: &mut Vec<(String, String, Expr)>) {
    match expr {
        Expr::Perform {
            effect, op, args, ..
        } if effect == "Web" && op == "route" && args.len() == 3 => {
            let method = string_literal(&args[0]).unwrap_or_else(|| "GET".to_string());
            let path = string_literal(&args[1]).unwrap_or_default();
            out.push((method, path, args[2].clone()));
        }
        _ => {}
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
            for e in exprs {
                collect_routes_in_expr(e, out);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) | Expr::FString(exprs, ..) => {
            for e in exprs {
                collect_routes_in_expr(e, out);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, e) in fields {
                collect_routes_in_expr(e, out);
            }
        }
        Expr::FieldAccess { expr, .. } => collect_routes_in_expr(expr, out),
        Expr::Index { arr, idx, .. } => {
            collect_routes_in_expr(arr, out);
            collect_routes_in_expr(idx, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_routes_in_expr(left, out);
            collect_routes_in_expr(right, out);
        }
        Expr::Unary { expr, .. } => collect_routes_in_expr(expr, out),
        Expr::Assign { target, value, .. } => {
            collect_routes_in_expr(target, out);
            collect_routes_in_expr(value, out);
        }
        Expr::Pipe { left, right, .. } => {
            collect_routes_in_expr(left, out);
            collect_routes_in_expr(right, out);
        }
        Expr::For { iterable, body, .. } => {
            collect_routes_in_expr(iterable, out);
            collect_routes_in_expr(body, out);
        }
        Expr::While { cond, body, .. } => {
            collect_routes_in_expr(cond, out);
            collect_routes_in_expr(body, out);
        }
        Expr::Return(e, ..) => {
            if let Some(e) = e {
                collect_routes_in_expr(e, out);
            }
        }
        Expr::Break(e, ..) => {
            if let Some(e) = e {
                collect_routes_in_expr(e, out);
            }
        }
        Expr::Consume { expr, .. } => collect_routes_in_expr(expr, out),
        Expr::Recover { body, .. } => collect_routes_in_expr(body, out),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            collect_routes_in_expr(expr, out);
        }
        Expr::Handle { body, handlers, .. } => {
            collect_routes_in_expr(body, out);
            for h in handlers {
                collect_routes_in_expr(&h.body, out);
            }
        }
        Expr::Perform { args, .. } => {
            for arg in args {
                collect_routes_in_expr(arg, out);
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
            for (_, e) in init {
                collect_routes_in_expr(e, out);
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
            if let Some((t, b)) = after {
                collect_routes_in_expr(t, out);
                collect_routes_in_expr(b, out);
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

/// Extract parameter names from a route path like `/users/:id/posts/:post_id`.
fn parse_path_params(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|segment| {
            if segment.starts_with(':') {
                Some(segment[1..].to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Collect all `perform Web.param("name")` reads in an expression.
fn collect_param_reads(expr: &Expr) -> Vec<String> {
    let mut reads = Vec::new();
    collect_param_reads_rec(expr, &mut reads);
    reads
}

fn collect_param_reads_rec(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        // Direct effect: perform Web.param("name")
        Expr::Perform {
            effect, op, args, ..
        } if effect == "Web" && op == "param" && !args.is_empty() => {
            if let Some(name) = string_literal(&args[0]) {
                out.push(name);
            }
        }
        // Stdlib wrapper: param("name"). This is a heuristic; it may match a
        // local function named `param`, but that is rare in web handlers.
        Expr::App { func, args, .. } if is_var(func, "param") && args.len() == 1 => {
            if let Some(name) = string_literal(&args[0]) {
                out.push(name);
            }
        }
        _ => {}
    }

    match expr {
        Expr::Lambda { body, .. } => collect_param_reads_rec(body, out),
        Expr::App { func, args, .. } => {
            collect_param_reads_rec(func, out);
            for arg in args {
                collect_param_reads_rec(arg, out);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_param_reads_rec(value, out);
            collect_param_reads_rec(body, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_param_reads_rec(cond, out);
            collect_param_reads_rec(then_branch, out);
            if let Some(else_branch) = else_branch {
                collect_param_reads_rec(else_branch, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_param_reads_rec(scrutinee, out);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_param_reads_rec(guard, out);
                }
                collect_param_reads_rec(body, out);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for e in exprs {
                collect_param_reads_rec(e, out);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) | Expr::FString(exprs, ..) => {
            for e in exprs {
                collect_param_reads_rec(e, out);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, e) in fields {
                collect_param_reads_rec(e, out);
            }
        }
        Expr::FieldAccess { expr, .. } => collect_param_reads_rec(expr, out),
        Expr::Index { arr, idx, .. } => {
            collect_param_reads_rec(arr, out);
            collect_param_reads_rec(idx, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_param_reads_rec(left, out);
            collect_param_reads_rec(right, out);
        }
        Expr::Unary { expr, .. } => collect_param_reads_rec(expr, out),
        Expr::Assign { target, value, .. } => {
            collect_param_reads_rec(target, out);
            collect_param_reads_rec(value, out);
        }
        Expr::Pipe { left, right, .. } => {
            collect_param_reads_rec(left, out);
            collect_param_reads_rec(right, out);
        }
        Expr::For { iterable, body, .. } => {
            collect_param_reads_rec(iterable, out);
            collect_param_reads_rec(body, out);
        }
        Expr::While { cond, body, .. } => {
            collect_param_reads_rec(cond, out);
            collect_param_reads_rec(body, out);
        }
        Expr::Return(e, ..) => {
            if let Some(e) = e {
                collect_param_reads_rec(e, out);
            }
        }
        Expr::Break(e, ..) => {
            if let Some(e) = e {
                collect_param_reads_rec(e, out);
            }
        }
        Expr::Consume { expr, .. } => collect_param_reads_rec(expr, out),
        Expr::Recover { body, .. } => collect_param_reads_rec(body, out),
        Expr::CapAnnotate { expr, .. } | Expr::TypeAnnotate { expr, .. } => {
            collect_param_reads_rec(expr, out);
        }
        Expr::Handle { body, handlers, .. } => {
            collect_param_reads_rec(body, out);
            for h in handlers {
                collect_param_reads_rec(&h.body, out);
            }
        }
        Expr::Perform { args, .. } => {
            for arg in args {
                collect_param_reads_rec(arg, out);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                collect_param_reads_rec(arg, out);
            }
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_param_reads_rec(actor_type, out);
            for (_, e) in init {
                collect_param_reads_rec(e, out);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_param_reads_rec(arg, out);
                }
            }
            if let Some(target) = target_node {
                collect_param_reads_rec(target, out);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_param_reads_rec(actor, out);
            for arg in args {
                collect_param_reads_rec(arg, out);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_param_reads_rec(guard, out);
                }
                collect_param_reads_rec(body, out);
            }
            if let Some((t, b)) = after {
                collect_param_reads_rec(t, out);
                collect_param_reads_rec(b, out);
            }
        }
        Expr::GrainRef { key, .. } => collect_param_reads_rec(key, out),
        Expr::Resume { value, .. } => collect_param_reads_rec(value, out),
        Expr::Migrate { actor, node, .. } => {
            collect_param_reads_rec(actor, out);
            collect_param_reads_rec(node, out);
        }
        _ => {}
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(s), ..) => Some(s.clone()),
        _ => None,
    }
}

fn is_var(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Var(n, ..) if n == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> AstModule {
        let tokens = Lexer::new(source).lex().unwrap();
        Parser::new(tokens).parse_module().unwrap()
    }

    #[test]
    fn test_missing_param_read() {
        let module = parse(
            r#"
fn product_page() {
    let _ = perform Web.param("id")
    text("ok")
}

fn web_main() {
    perform Web.route("GET", "/products/:id", product_page)
}
"#,
        );
        let diag = check_module(&module);
        assert!(diag.is_empty(), "expected no diagnostics, got {:?}", diag);
    }

    #[test]
    fn test_route_param_not_read() {
        let module = parse(
            r#"
fn product_page() {
    text("ok")
}

fn web_main() {
    perform Web.route("GET", "/products/:id", product_page)
}
"#,
        );
        let diag = check_module(&module);
        assert_eq!(diag.len(), 1);
        assert!(diag[0].message.contains("declares parameter ':id'"));
    }

    #[test]
    fn test_handler_reads_undeclared_param() {
        let module = parse(
            r#"
fn product_page() {
    let _ = perform Web.param("id")
    let _ = perform Web.param("slug")
    text("ok")
}

fn web_main() {
    perform Web.route("GET", "/products/:id", product_page)
}
"#,
        );
        let diag = check_module(&module);
        assert_eq!(diag.len(), 1);
        assert!(diag[0].message.contains("does not declare ':slug'"));
    }

    #[test]
    fn test_multiple_params() {
        let module = parse(
            r#"
fn user_post() {
    let uid = perform Web.param("uid")
    let pid = perform Web.param("pid")
    text("ok")
}

fn web_main() {
    perform Web.route("GET", "/users/:uid/posts/:pid", user_post)
}
"#,
        );
        let diag = check_module(&module);
        assert!(diag.is_empty(), "expected no diagnostics, got {:?}", diag);
    }

    #[test]
    fn test_param_wrapper_detected() {
        let module = parse(
            r#"
fn product_page() {
    let id = param("id")
    text("ok")
}

fn web_main() {
    perform Web.route("GET", "/products/:id", product_page)
}
"#,
        );
        let diag = check_module(&module);
        assert!(diag.is_empty(), "expected no diagnostics, got {:?}", diag);
    }
}
