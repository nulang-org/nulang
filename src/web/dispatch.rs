//! Transport-facing dispatch seam for typed Nulang Web routes.
//!
//! HTTP remains responsible for request lifecycle, headers, cookies, and the
//! current legacy request context. This module owns only the compiler-derived
//! pieces: validating package contracts, attaching them to runtime route
//! registrations, matching precompiled path segments, and invoking handlers
//! whose complete argument list is proven by the binding plan.

use crate::runtime::WebRoute;
use crate::web::package_contracts::compile_contracts_from_tree;
use crate::web::runtime_bindings::{
    attach_runtime_route_plans, match_attached_route, render_bound_route_handler, RuntimeWebRoute,
};
use std::collections::HashMap;
use std::path::Path;

/// Compile package web contracts and attach them to routes collected by the VM.
///
/// This is the intended package/dev-server boundary. Invalid contract-first
/// routes fail before request serving starts; legacy routes without a compiler
/// contract remain available through their existing runtime representation.
pub fn compile_runtime_routes(
    routes: Vec<WebRoute>,
    src_root: &Path,
) -> Result<Vec<RuntimeWebRoute>, Vec<String>> {
    let contracts = compile_contracts_from_tree(src_root);
    let attachment = attach_runtime_route_plans(routes, &contracts);
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

    #[test]
    fn empty_package_compiles_to_empty_runtime_route_set() {
        let dir =
            std::env::temp_dir().join(format!("nulang_web_dispatch_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let routes = compile_runtime_routes(Vec::new(), &dir).unwrap();
        assert!(routes.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
