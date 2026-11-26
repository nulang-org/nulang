//! Validation facade for Nulang Web route contracts.
//!
//! Contract extraction and argument binding are intentionally separate passes:
//! extraction describes source intent, while binding lowers request inputs to
//! handler ABI slots. Build/compiler entry points should consume this module so
//! they have one authoritative diagnostic list instead of reimplementing policy.

use crate::web::bindings::compile_route_bindings;
use crate::web::contracts::ContractCompilation;
use crate::web::package_contracts::compile_contracts_from_tree;
use crate::web::runtime_bindings::{compare_route_patterns, RoutePatternRelation};
use std::collections::HashSet;
use std::path::Path;

/// Compile and validate every statically extractable route contract in a
/// package source tree.
///
/// This is the preferred build/dev-server entry point. Returning the validated
/// compilation on success lets downstream consumers (runtime dispatch, IR,
/// OpenAPI/client generation) share the exact same compiler-owned contract set
/// instead of independently reparsing source and potentially disagreeing about
/// diagnostics.
pub fn compile_validated_contracts_from_tree(
    src_root: &Path,
) -> Result<ContractCompilation, Vec<String>> {
    validate_compilation(compile_contracts_from_tree(src_root))
}

/// Validate every statically extractable route contract in a package source
/// tree. The returned strings are deterministic and deduplicated.
pub fn validate_contracts_from_tree(src_root: &Path) -> Vec<String> {
    match compile_validated_contracts_from_tree(src_root) {
        Ok(_) => Vec::new(),
        Err(diagnostics) => diagnostics,
    }
}

/// Turn a best-effort contract compilation into a hard validation boundary.
///
/// Contract extraction deliberately retains diagnostics instead of panicking so
/// IDEs and tooling can inspect partial metadata. Build and runtime entry points,
/// however, should not serve or emit artifacts from an invalid contract-first
/// route. This helper makes that policy explicit while preserving the successful
/// compilation for all downstream consumers.
pub fn validate_compilation(
    compilation: ContractCompilation,
) -> Result<ContractCompilation, Vec<String>> {
    let diagnostics = validation_diagnostics(&compilation);
    if diagnostics.is_empty() {
        Ok(compilation)
    } else {
        Err(diagnostics)
    }
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

    // Apply the same specificity relation used by HTTP dispatch. A literal
    // subroute can safely shadow a parameter route; two overlapping routes
    // that are incomparable must fail during compilation, not at runtime.
    for (index, left) in compilation.routes.iter().enumerate() {
        for right in compilation.routes.iter().skip(index + 1) {
            if left.method != right.method {
                continue;
            }
            if let Ok(RoutePatternRelation::Ambiguous) =
                compare_route_patterns(&left.path, &right.path)
            {
                diagnostics.push(format!(
                    "ambiguous {} routes '{}' and '{}': neither pattern is strictly more specific",
                    left.method, left.path, right.path
                ));
            }
        }
    }

    let mut seen = HashSet::new();
    diagnostics.retain(|diagnostic| seen.insert(diagnostic.clone()));
    diagnostics.sort();
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{HandlerParamContract, RouteContract, RouteParamContract};

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
                    request: None,
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

    #[test]
    fn validate_compilation_preserves_valid_contracts() {
        let compilation = ContractCompilation {
            routes: vec![contract(
                "/users/{id: UserId}",
                vec![HandlerParamContract {
                    name: "id".to_string(),
                    ty: Some("UserId".to_string()),
                    capability: None,
                    request: None,
                }],
            )],
            diagnostics: Vec::new(),
        };

        let validated = validate_compilation(compilation.clone()).unwrap();
        assert_eq!(validated, compilation);
    }

    #[test]
    fn validate_compilation_rejects_invalid_contracts() {
        let compilation = ContractCompilation {
            routes: vec![contract("/users/{id: UserId}", Vec::new())],
            diagnostics: Vec::new(),
        };

        let diagnostics = validate_compilation(compilation).unwrap_err();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("has no same-named parameter"));
    }
    #[test]
    fn ambiguous_path_patterns_fail_validation_before_serving() {
        let mut first = contract("/users/{first}", Vec::new());
        first.params.clear();
        let mut second = contract("/users/{second}", Vec::new());
        second.params.clear();
        let diagnostics = validation_diagnostics(&ContractCompilation {
            routes: vec![first, second],
            diagnostics: Vec::new(),
        });
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("ambiguous"), "{diagnostics:?}");
    }

    #[test]
    fn crossing_literal_and_parameter_patterns_are_ambiguous() {
        let mut first = contract("/{a}/fixed", Vec::new());
        first.params.clear();
        let mut second = contract("/fixed/{b}", Vec::new());
        second.params.clear();
        let diagnostics = validation_diagnostics(&ContractCompilation {
            routes: vec![first, second],
            diagnostics: Vec::new(),
        });
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("ambiguous"), "{diagnostics:?}");
    }

    #[test]
    fn strict_literal_specificity_and_different_methods_are_allowed() {
        let mut parameter = contract("/users/{id}", Vec::new());
        parameter.params.clear();
        let mut literal = contract("/users/me", Vec::new());
        literal.params.clear();
        let mut post = parameter.clone();
        post.method = "POST".into();
        let diagnostics = validation_diagnostics(&ContractCompilation {
            routes: vec![parameter, literal, post],
            diagnostics: Vec::new(),
        });
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }
}
