//! Validation facade for Nulang Web route contracts.
//!
//! Contract extraction and argument binding are intentionally separate passes:
//! extraction describes source intent, while binding lowers request inputs to
//! handler ABI slots. Build/compiler entry points should consume this module so
//! they have one authoritative diagnostic list instead of reimplementing policy.

use crate::web::bindings::compile_route_bindings;
use crate::web::contracts::ContractCompilation;
use crate::web::package_contracts::compile_contracts_from_tree;
use std::collections::HashSet;
use std::path::Path;

/// Validate every statically extractable route contract in a package source
/// tree. The returned strings are deterministic and deduplicated.
pub fn validate_contracts_from_tree(src_root: &Path) -> Vec<String> {
    let compilation = compile_contracts_from_tree(src_root);
    validation_diagnostics(&compilation)
}

/// Aggregate extraction diagnostics and route-binding diagnostics.
///
/// Extraction owns malformed paths and explicit route/handler type conflicts.
/// Binding owns contract-first input-to-handler requirements. Keeping this
/// aggregation explicit lets callers decide whether diagnostics are warnings,
/// IDE messages, or hard build errors without coupling that policy to IR JSON.
pub fn validation_diagnostics(compilation: &ContractCompilation) -> Vec<String> {
    let mut diagnostics = compilation.diagnostics.clone();
    for route in &compilation.routes {
        diagnostics.extend(compile_route_bindings(route).diagnostics);
    }

    let mut seen = HashSet::new();
    diagnostics.retain(|diagnostic| seen.insert(diagnostic.clone()));
    diagnostics.sort();
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{
        HandlerParamContract, RouteContract, RouteParamContract,
    };

    fn contract(path: &str, handler_params: Vec<HandlerParamContract>) -> RouteContract {
        RouteContract {
            method: "GET".to_string(),
            path: path.to_string(),
            handler: Some("show_user".to_string()),
            params: vec![RouteParamContract {
                name: "id".to_string(),
                ty: Some("UserId".to_string()),
            }],
            handler_params,
            response_type: Some("String".to_string()),
            error_type: None,
            effects: Vec::new(),
            reference_capability: None,
            placement: Some("server".to_string()),
        }
    }

    #[test]
    fn aggregates_contract_and_binding_diagnostics() {
        let compilation = ContractCompilation {
            routes: vec![contract("/users/{id: UserId}", Vec::new())],
            diagnostics: vec!["malformed route fixture".to_string()],
        };

        let diagnostics = validation_diagnostics(&compilation);
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic == "malformed route fixture"));
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("has no same-named parameter")));
    }

    #[test]
    fn legacy_ambient_param_fallback_is_not_a_validation_error() {
        let compilation = ContractCompilation {
            routes: vec![contract("/users/:id", Vec::new())],
            diagnostics: Vec::new(),
        };
        assert!(validation_diagnostics(&compilation).is_empty());
    }

    #[test]
    fn typed_binding_with_matching_handler_is_valid() {
        let compilation = ContractCompilation {
            routes: vec![contract(
                "/users/{id: UserId}",
                vec![HandlerParamContract {
                    name: "id".to_string(),
                    ty: Some("UserId".to_string()),
                    capability: None,
                }],
            )],
            diagnostics: Vec::new(),
        };
        assert!(validation_diagnostics(&compilation).is_empty());
    }

    #[test]
    fn diagnostics_are_deduplicated_and_sorted() {
        let compilation = ContractCompilation {
            routes: Vec::new(),
            diagnostics: vec![
                "z diagnostic".to_string(),
                "a diagnostic".to_string(),
                "z diagnostic".to_string(),
            ],
        };
        assert_eq!(
            validation_diagnostics(&compilation),
            vec!["a diagnostic".to_string(), "z diagnostic".to_string()]
        );
    }
}
