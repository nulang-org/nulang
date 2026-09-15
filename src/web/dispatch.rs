//! Transport-facing dispatch seam for typed Nulang Web routes.
//!
//! HTTP remains responsible for request lifecycle, headers, cookies, and the
//! current legacy request context. This module owns only the compiler-derived
//! pieces: validating package contracts, attaching them to runtime route
//! registrations, matching precompiled path segments, and invoking handlers
//! whose complete argument list is proven by the binding plan.

use crate::runtime::WebRoute;
use crate::web::bindings::RouteBindingSource;
use crate::web::contracts::ContractCompilation;
use crate::web::handler_call::render_bound_handler;
use crate::web::request_bindings::{
    bind_request_arguments, RequestBindingValues, RequestDecodeError,
};
use crate::web::runtime_bindings::{
    attach_runtime_route_plans, match_attached_route, render_bound_route_handler, RuntimeRoutePlan,
    RuntimeRouteSegment, RuntimeWebRoute,
};
use crate::web::validation::compile_validated_contracts_from_tree;
use std::collections::HashMap;
use std::fmt;
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

/// Match one attached runtime route against an HTTP request target.
///
/// HTTP request targets may contain a query string (`/users?limit=10`), while
/// route plans describe only the path component. Strip the query portion before
/// matching so both compiler-backed and legacy routes have identical semantics.
pub fn match_route(
    route: &RuntimeWebRoute,
    request_target: &str,
) -> Option<HashMap<String, String>> {
    match_attached_route(route, request_path_only(request_target))
}

fn request_path_only(target: &str) -> &str {
    target.split_once('?').map_or(target, |(path, _)| path)
}

/// Invoke a path-only route directly when the existing runtime plan marks it as
/// fully compiler-bound.
///
/// This remains the backwards-compatible path-specific API. New transports that
/// have captured all request sources should use [`render_direct_request`].
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

/// Failure while invoking a complete multi-source request binding plan.
#[derive(Debug)]
pub enum DirectRequestRenderError {
    Decode(RequestDecodeError),
    Execution(String),
}

impl fmt::Display for DirectRequestRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirectRequestRenderError::Decode(error) => write!(f, "{error}"),
            DirectRequestRenderError::Execution(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for DirectRequestRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DirectRequestRenderError::Decode(error) => Some(error),
            DirectRequestRenderError::Execution(_) => None,
        }
    }
}

/// Invoke a route from a complete set of compiler-bound request inputs.
///
/// `Ok(None)` preserves the legacy execution path when the route still depends
/// on ambient request state. Once the compiler emits query/header/cookie/body/
/// form bindings, this function can execute them without any source-specific VM
/// logic: decoding happens in `bind_request_arguments`, and `handler_call` only
/// sees the final handler-slot argument bank.
pub fn render_direct_request(
    route: &RuntimeWebRoute,
    values: &RequestBindingValues<'_>,
) -> Result<Option<String>, DirectRequestRenderError> {
    let Some(plan) = route.plan.as_ref() else {
        return Ok(None);
    };
    if !has_complete_request_binding_plan(plan) {
        return Ok(None);
    }

    let args = bind_request_arguments(&plan.bindings, plan.handler_param_count, values)
        .map_err(DirectRequestRenderError::Decode)?;
    render_bound_handler(
        &route.route.handler_module,
        route.route.handler_func_idx,
        plan.handler_param_count,
        &args,
    )
    .map(Some)
    .map_err(DirectRequestRenderError::Execution)
}

/// A generalized direct call is safe only when every handler slot has exactly
/// one binding and every path capture is also supplied by a path binding.
///
/// The second condition matters during migration: a legacy handler may bind all
/// of its declared parameters from query/header inputs while still reading a
/// `:path` value through ambient `Web.param`. Such a route must stay on the
/// legacy request-context path rather than being mistaken for a complete direct
/// call merely because the binding/parameter counts happen to match.
fn has_complete_request_binding_plan(plan: &RuntimeRoutePlan) -> bool {
    if plan.bindings.len() != plan.handler_param_count {
        return false;
    }

    let mut handler_slots = vec![false; plan.handler_param_count];
    for binding in &plan.bindings {
        if binding.handler_index >= plan.handler_param_count || handler_slots[binding.handler_index]
        {
            return false;
        }
        handler_slots[binding.handler_index] = true;
    }
    if handler_slots.iter().any(|bound| !bound) {
        return false;
    }

    plan.segments.iter().all(|segment| match segment {
        RuntimeRouteSegment::Literal(_) => true,
        RuntimeRouteSegment::PathParam(name) => plan.bindings.iter().any(|binding| {
            binding.source == RouteBindingSource::Path && binding.source_name == *name
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::bindings::RouteBindingContract;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_web_dispatch_{}_{}",
            label,
            std::process::id()
        ))
    }

    fn binding(
        source: RouteBindingSource,
        source_name: &str,
        handler_index: usize,
    ) -> RouteBindingContract {
        RouteBindingContract {
            source,
            source_name: source_name.to_string(),
            handler_param: format!("arg_{handler_index}"),
            handler_index,
            ty: Some("String".to_string()),
        }
    }

    fn plan(bindings: Vec<RouteBindingContract>, handler_param_count: usize) -> RuntimeRoutePlan {
        RuntimeRoutePlan {
            method: "GET".to_string(),
            path: "/users/{id}".to_string(),
            segments: vec![
                RuntimeRouteSegment::Literal("users".to_string()),
                RuntimeRouteSegment::PathParam("id".to_string()),
            ],
            bindings,
            handler_param_count,
            direct_call: false,
        }
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

    #[test]
    fn request_target_query_is_not_part_of_route_path() {
        assert_eq!(request_path_only("/users?limit=10"), "/users");
        assert_eq!(request_path_only("/users/42?expand=true"), "/users/42");
        assert_eq!(request_path_only("/users/42"), "/users/42");
    }

    #[test]
    fn complete_request_plan_accepts_multiple_request_sources() {
        let complete = plan(
            vec![
                binding(RouteBindingSource::Path, "id", 0),
                binding(RouteBindingSource::Query, "expand", 1),
            ],
            2,
        );
        assert!(has_complete_request_binding_plan(&complete));
    }

    #[test]
    fn ambient_legacy_path_prevents_generalized_direct_call() {
        let query_only = plan(vec![binding(RouteBindingSource::Query, "id", 0)], 1);
        assert!(!has_complete_request_binding_plan(&query_only));
    }

    #[test]
    fn incomplete_or_duplicate_handler_slots_are_not_direct() {
        let incomplete = plan(vec![binding(RouteBindingSource::Path, "id", 0)], 2);
        assert!(!has_complete_request_binding_plan(&incomplete));

        let duplicate = plan(
            vec![
                binding(RouteBindingSource::Path, "id", 0),
                binding(RouteBindingSource::Query, "expand", 0),
            ],
            2,
        );
        assert!(!has_complete_request_binding_plan(&duplicate));
    }
}
