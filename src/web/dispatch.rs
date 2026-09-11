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
    attach_runtime_route_plans, match_attached_route, render_bound_route_handler,
    RuntimeWebRoute,
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
pub fn match_route(
    route: &RuntimeWebRoute,
    request_path: &str,
) -> Option<HashMap<String, String>> {
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
    use crate::bytecode::CodeModule;
    use crate::runtime::HttpMethod;
    use crate::web::runtime_bindings::{RuntimeRoutePlan, RuntimeRouteSegment};

    fn raw_route(path: &str) -> WebRoute {
        WebRoute {
            method: HttpMethod::Get,
            path: path.to_string(),
            handler_module: CodeModule::new("dispatch-test"),
            handler_func_idx: 0,
        }
    }

    #[test]
    fn legacy_route_without_plan_uses_legacy_matcher() {
        let route = RuntimeWebRoute {
            route: raw_route("/users/:id"),
            plan: None,
        };
        let params = match_route(&route, "/users/42").unwrap();
        assert_eq!(params.get("id"), Some(&"42".to_string()));
        assert!(render_direct_route(&route, &params).unwrap().is_none());
    }

    #[test]
    fn compiler_plan_enables_brace_matching_without_legacy_parser_changes() {
        let route = RuntimeWebRoute {
            route: raw_route("/users/{id: Int}"),
            plan: Some(RuntimeRoutePlan {
                method: "GET".to_string(),
                path: "/users/{id: Int}".to_string(),
                segments: vec![
                    RuntimeRouteSegment::Literal("users".to_string()),
                    RuntimeRouteSegment::PathParam("id".to_string()),
                ],
                bindings: Vec::new(),
                handler_param_count: 0,
                direct_call: false,
            }),
        };
        let params = match_route(&route, "/users/42").unwrap();
        assert_eq!(params.get("id"), Some(&"42".to_string()));
    }
}
