//! Typed route-to-handler binding plans.
//!
//! Route contracts describe the source-level endpoint and handler signature.
//! This module lowers that metadata one step further into an explicit argument
//! binding plan that runtimes and generated adapters can consume without
//! re-parsing route strings or depending on ambient request state.
//!
//! Legacy `:name` routes remain backwards compatible: when a same-named handler
//! parameter exists we emit a direct binding, otherwise the route may continue
//! to use `Web.param("name")`. Contract-first `{name}` / `{name: Type}` segments
//! require a same-named handler parameter and produce a diagnostic when one is
//! missing.

use crate::web::contracts::RouteContract;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Request source used to populate a handler argument.
///
/// Only path parameters are lowered today. Keeping the source explicit avoids
/// baking positional conventions into the IR when query/body/header bindings
/// are added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteBindingSource {
    Path,
}

/// One deterministic handler-argument binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteBindingContract {
    pub source: RouteBindingSource,
    pub source_name: String,
    pub handler_param: String,
    pub handler_index: usize,
    /// Resolved source-level type, when known.
    pub ty: Option<String>,
}

/// Result of lowering a route contract into direct handler bindings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingCompilation {
    pub bindings: Vec<RouteBindingContract>,
    pub diagnostics: Vec<String>,
}

/// Build a deterministic binding plan for a route contract.
///
/// The resulting vector is ordered by handler parameter index, not path order,
/// so a runtime can stage arguments directly into the call ABI. A handler may
/// declare parameters in a different order than they appear in the URL.
pub fn compile_route_bindings(contract: &RouteContract) -> BindingCompilation {
    let contract_params = contract_syntax_params(&contract.path);
    let mut out = BindingCompilation::default();

    for route_param in &contract.params {
        let Some((handler_index, handler_param)) = contract
            .handler_params
            .iter()
            .enumerate()
            .find(|(_, param)| param.name == route_param.name)
        else {
            if contract_params.contains(route_param.name.as_str()) {
                out.diagnostics.push(format!(
                    "{} {}: contract route parameter '{}' has no same-named parameter on handler '{}'",
                    contract.method,
                    contract.path,
                    route_param.name,
                    contract.handler.as_deref().unwrap_or("<handler>")
                ));
            }
            continue;
        };

        out.bindings.push(RouteBindingContract {
            source: RouteBindingSource::Path,
            source_name: route_param.name.clone(),
            handler_param: handler_param.name.clone(),
            handler_index,
            ty: route_param.ty.clone().or_else(|| handler_param.ty.clone()),
        });
    }

    out.bindings.sort_by_key(|binding| binding.handler_index);
    out
}

/// Return names introduced with contract-first brace syntax.
fn contract_syntax_params(path: &str) -> HashSet<&str> {
    path.split('/')
        .filter_map(|segment| {
            let inner = segment.strip_prefix('{')?.strip_suffix('}')?;
            let name = inner.split_once(':').map_or(inner, |(name, _)| name).trim();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{HandlerParamContract, RouteParamContract};

    fn route(path: &str) -> RouteContract {
        RouteContract {
            method: "GET".to_string(),
            path: path.to_string(),
            handler: Some("show_user".to_string()),
            params: vec![RouteParamContract {
                name: "id".to_string(),
                ty: Some("UserId".to_string()),
            }],
            handler_params: vec![HandlerParamContract {
                name: "id".to_string(),
                ty: Some("UserId".to_string()),
                capability: None,
            }],
            response_type: Some("String".to_string()),
            error_type: None,
            effects: Vec::new(),
            reference_capability: None,
            placement: Some("server".to_string()),
        }
    }

    #[test]
    fn typed_path_binds_to_handler_slot() {
        let compiled = compile_route_bindings(&route("/users/{id: UserId}"));
        assert!(compiled.diagnostics.is_empty());
        assert_eq!(compiled.bindings.len(), 1);
        assert_eq!(compiled.bindings[0].source, RouteBindingSource::Path);
        assert_eq!(compiled.bindings[0].source_name, "id");
        assert_eq!(compiled.bindings[0].handler_param, "id");
        assert_eq!(compiled.bindings[0].handler_index, 0);
        assert_eq!(compiled.bindings[0].ty.as_deref(), Some("UserId"));
    }

    #[test]
    fn legacy_path_can_opt_into_direct_binding() {
        let compiled = compile_route_bindings(&route("/users/:id"));
        assert!(compiled.diagnostics.is_empty());
        assert_eq!(compiled.bindings.len(), 1);
    }

    #[test]
    fn legacy_path_without_handler_param_keeps_ambient_fallback() {
        let mut contract = route("/users/:id");
        contract.handler_params.clear();
        let compiled = compile_route_bindings(&contract);
        assert!(compiled.bindings.is_empty());
        assert!(compiled.diagnostics.is_empty());
    }

    #[test]
    fn contract_path_requires_handler_param() {
        let mut contract = route("/users/{id}");
        contract.handler_params.clear();
        let compiled = compile_route_bindings(&contract);
        assert!(compiled.bindings.is_empty());
        assert_eq!(compiled.diagnostics.len(), 1);
        assert!(compiled.diagnostics[0].contains("has no same-named parameter"));
    }

    #[test]
    fn bindings_are_ordered_by_handler_slot() {
        let contract = RouteContract {
            method: "GET".to_string(),
            path: "/orgs/{org}/users/{user}".to_string(),
            handler: Some("show".to_string()),
            params: vec![
                RouteParamContract {
                    name: "org".to_string(),
                    ty: Some("OrgId".to_string()),
                },
                RouteParamContract {
                    name: "user".to_string(),
                    ty: Some("UserId".to_string()),
                },
            ],
            handler_params: vec![
                HandlerParamContract {
                    name: "user".to_string(),
                    ty: Some("UserId".to_string()),
                    capability: None,
                },
                HandlerParamContract {
                    name: "org".to_string(),
                    ty: Some("OrgId".to_string()),
                    capability: None,
                },
            ],
            response_type: None,
            error_type: None,
            effects: Vec::new(),
            reference_capability: None,
            placement: None,
        };

        let compiled = compile_route_bindings(&contract);
        assert!(compiled.diagnostics.is_empty());
        assert_eq!(compiled.bindings[0].source_name, "user");
        assert_eq!(compiled.bindings[0].handler_index, 0);
        assert_eq!(compiled.bindings[1].source_name, "org");
        assert_eq!(compiled.bindings[1].handler_index, 1);
    }
}
