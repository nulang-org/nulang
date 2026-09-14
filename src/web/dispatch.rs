//! Transport-facing dispatch seam for typed Nulang Web routes.
//!
//! HTTP remains responsible for request lifecycle, headers, cookies, and the
//! current legacy request context. This module owns only the compiler-derived
//! pieces: validating package contracts, attaching them to runtime route
//! registrations, matching precompiled path segments, and invoking handlers
//! whose complete argument list is proven by the binding plan.

use crate::runtime::WebRoute;
use crate::web::contracts::ContractCompilation;
use crate::web::runtime_bindings::{
    attach_runtime_route_plans, match_attached_route, render_bound_route_handler, RuntimeWebRoute,
};
use crate::web::validation::compile_validated_contracts_from_tree;
use std::collections::HashMap;
use std::path::Path;

/// Compile, validate, and attach package web contracts to routes collected by
/// the VM.
///
/// This is the intended package/dev-server boundary. Contract-first routes are
/// a hard validation boundary: malformed or incomplete contracts fail before
/// request serving starts. Legacy routes without a compiler contract remain
/// available through their existing runtime representation.
pub fn compile_runtime_routes(
    routes: Vec<WebRoute>,
    src_root: &Path,
) -> Result<Vec<RuntimeWebRoute>, Vec<String>> {
    let contracts = compile_validated_contracts_from_tree(src_root)?;
    compile_runtime_routes_from_contracts(routes, &contracts)
}

/// Attach a previously validated compiler contract set to routes collected by
/// the VM.
///
/// Build/dev tooling that already owns a [`ContractCompilation`] should use this
/// entry point instead of reparsing source. Keeping one compiler-owned contract
/// value allows runtime dispatch, deployment IR, OpenAPI/client generation, and
/// tests to consume identical metadata.
pub fn compile_runtime_routes_from_contracts(
    routes: Vec<WebRoute>,
    contracts: &ContractCompilation,
) -> Result<Vec<RuntimeWebRoute>, Vec<String>> {
    let attachment = attach_runtime_route_plans(routes, contracts);
    if attachment.diagnostics.is_empty() {
        Ok(attachment.routes)
    } else {
        Err(attachment.diagnostics)
    }
}

/// Match one attached runtime route against a request path.
///
/// Contract-backed routes use their precompiled route segments. Legacy routes
/// fall back to the existing `:name` convention.
pub fn match_route(route: &RuntimeWebRoute, request_path: &str) -> Option<HashMap<String, String>> {
    match_attached_route(route, request_path)
}

/// Invoke a route directly when the compiler proved every handler parameter has
/// a request binding.
///
/// `Ok(None)` means the route intentionally stays on the legacy execution path
/// (for example, a handler that still reads `Web.param` ambiently). The HTTP
/// transport should then use its existing request-context renderer. A direct
/// route never silently falls back after a typed binding or decode error.
pub fn render_direct_route(
    route: &RuntimeWebRoute,
    params: &HashMap<String, String>,
) -> Result<Option<String>, String> {
    let Some(plan) = route.plan.as_ref() else {
        return Ok(None);
    };
    if !plan.direct_call {
        return Ok(None);
    }

    render_bound_route_handler(
        &route.route.handler_module,
        route.route.handler_func_idx,
        &plan.bindings,
        plan.handler_param_count,
        params,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_web_dispatch_{}_{}_{}",
            label,
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ))
    }

    #[test]
    fn empty_package_compiles_to_empty_runtime_route_set() {
        let dir = temp_dir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let routes = compile_runtime_routes(Vec::new(), &dir).unwrap();
        assert!(routes.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_contract_fails_before_runtime_attachment() {
        let dir = temp_dir("invalid_contract");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("main.nula"),
            r#"
app "test" {
    route "GET" "/users/{id: UserId}" -> show
}

fn show() -> String { "ok" }
"#,
        )
        .unwrap();

        let diagnostics = compile_runtime_routes(Vec::new(), &dir).unwrap_err();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("no same-named typed parameter")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validated_contract_set_can_be_reused_without_source_reparse() {
        let contracts = ContractCompilation::default();
        let routes = compile_runtime_routes_from_contracts(Vec::new(), &contracts).unwrap();
        assert!(routes.is_empty());
    }
}
