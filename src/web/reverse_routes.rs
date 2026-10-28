//! Reverse routing compiled from validated Web Contract IR.
//!
//! Matching and URL generation share the same segment parser, so percent
//! encoding and parameter boundaries cannot silently diverge.
use crate::web::contracts::ContractCompilation;
use crate::web::runtime_bindings::{compile_route_segments, RuntimeRouteSegment};
use crate::web::validation::validate_compilation;
use std::collections::{HashMap, HashSet};

/// Immutable reverse-routing table built at application startup.
#[derive(Debug, Clone, Default)]
pub struct NamedRouteIndex {
    routes: HashMap<String, Vec<RuntimeRouteSegment>>,
}

impl NamedRouteIndex {
    /// Refuse incomplete or ambiguous contracts before registering any names.
    pub fn compile(compilation: &ContractCompilation) -> Result<Self, Vec<String>> {
        validate_compilation(compilation.clone())?;
        let mut routes = HashMap::new();
        for contract in &compilation.routes {
            if let Some(name) = &contract.name {
                let segments = compile_route_segments(&contract.path).map_err(|error| {
                    vec![format!(
                        "route {} {}: {error}",
                        contract.method, contract.path
                    )]
                })?;
                routes.insert(name.clone(), segments);
            }
        }
        Ok(Self { routes })
    }

    /// Generate an absolute-path reference, never an attacker-supplied origin.
    /// Reject missing/extra values and slash/dot segments before encoding.
    pub fn url_for(&self, name: &str, params: &HashMap<String, String>) -> Result<String, String> {
        let segments = self
            .routes
            .get(name)
            .ok_or_else(|| format!("unknown route name '{name}'"))?;
        let expected: HashSet<&str> = segments
            .iter()
            .filter_map(|segment| match segment {
                RuntimeRouteSegment::PathParam(name) => Some(name.as_str()),
                RuntimeRouteSegment::Literal(_) => None,
            })
            .collect();
        for key in params.keys() {
            if !expected.contains(key.as_str()) {
                return Err(format!("unexpected parameter '{key}' for route '{name}'"));
            }
        }
        let mut encoded = Vec::with_capacity(segments.len());
        for segment in segments {
            match segment {
                RuntimeRouteSegment::Literal(literal) => {
                    encoded.push(percent_encode_segment(literal))
                }
                RuntimeRouteSegment::PathParam(key) => {
                    let value = params
                        .get(key)
                        .ok_or_else(|| format!("missing parameter '{key}' for route '{name}'"))?;
                    if value.is_empty()
                        || value == "."
                        || value == ".."
                        || value.contains('/')
                        || value.contains('\\')
                    {
                        return Err(format!("unsafe path segment for parameter '{key}'"));
                    }
                    encoded.push(percent_encode_segment(value));
                }
            }
        }
        Ok(format!("/{}", encoded.join("/")))
    }
}

fn percent_encode_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0xf) as usize] as char);
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{
        ContractCompilation, HandlerParamContract, RouteContract, RouteParamContract,
    };

    fn routes() -> ContractCompilation {
        ContractCompilation {
            routes: vec![
                RouteContract {
                    method: "GET".into(),
                    path: "/users/{id: String}".into(),
                    name: Some("users.show".into()),
                    params: vec![RouteParamContract {
                        name: "id".into(),
                        ty: Some("String".into()),
                    }],
                    handler_params: vec![HandlerParamContract {
                        name: "id".into(),
                        ty: Some("String".into()),
                        capability: None,
                        request: None,
                    }],
                    ..RouteContract::default()
                },
                RouteContract {
                    method: "GET".into(),
                    path: "/about".into(),
                    name: Some("about".into()),
                    ..RouteContract::default()
                },
            ],
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn reverse_routing_roundtrips_utf8_and_reserved_characters() {
        let index = NamedRouteIndex::compile(&routes()).unwrap();
        let params = HashMap::from([("id".to_string(), "Olá & 50%".to_string())]);
        let url = index.url_for("users.show", &params).unwrap();
        assert_eq!(url, "/users/Ol%C3%A1%20%26%2050%25");
        let segments = compile_route_segments("/users/{id: String}").unwrap();
        let plan = crate::web::runtime_bindings::RuntimeRoutePlan {
            method: "GET".into(),
            path: "/users/{id: String}".into(),
            segments,
            bindings: Vec::new(),
            handler_param_count: 0,
            direct_call: false,
        };
        assert_eq!(
            crate::web::runtime_bindings::match_runtime_route(&plan, &url).unwrap()["id"],
            "Olá & 50%"
        );
    }

    #[test]
    fn rejects_unknown_names_missing_extra_and_unsafe_parameters() {
        let index = NamedRouteIndex::compile(&routes()).unwrap();
        assert!(index.url_for("missing", &HashMap::new()).is_err());
        assert!(index.url_for("users.show", &HashMap::new()).is_err());
        assert!(index
            .url_for("about", &HashMap::from([("extra".into(), "1".into())]))
            .is_err());
        for value in ["", ".", "..", "a/b", "a\\b"] {
            assert!(
                index
                    .url_for("users.show", &HashMap::from([("id".into(), value.into())]))
                    .is_err(),
                "unsafe value: {value}"
            );
        }
        assert_eq!(index.url_for("about", &HashMap::new()).unwrap(), "/about");
    }

    #[test]
    fn rejects_unvalidated_duplicate_names() {
        let mut invalid = routes();
        invalid.routes[1].name = Some("users.show".into());
        assert!(NamedRouteIndex::compile(&invalid).is_err());
    }

    #[test]
    fn named_reverse_rejects_crossing_patterns_before_publishing_urls() {
        let mut compilation = routes();
        compilation.routes.clear();
        compilation.routes.push(RouteContract {
            method: "GET".into(),
            path: "/{area}/fixed".into(),
            name: Some("area.fixed".into()),
            ..RouteContract::default()
        });
        compilation.routes.push(RouteContract {
            method: "GET".into(),
            path: "/fixed/{user}".into(),
            name: Some("fixed.user".into()),
            ..RouteContract::default()
        });
        let errors = NamedRouteIndex::compile(&compilation)
            .expect_err("crossing named route patterns must fail rather than depend on order");
        assert!(
            errors.iter().any(|error| error.contains("ambiguous")),
            "{errors:?}"
        );
    }

    #[test]
    fn named_reverse_allows_literal_override_with_distinct_names() {
        let mut compilation = routes();
        compilation.routes.push(RouteContract {
            method: "GET".into(),
            path: "/users/me".into(),
            name: Some("users.current".into()),
            ..RouteContract::default()
        });
        let index = NamedRouteIndex::compile(&compilation)
            .expect("literal routes are more specific and should coexist");
        assert_eq!(
            index.url_for("users.current", &HashMap::new()).unwrap(),
            "/users/me"
        );
        assert_eq!(
            index
                .url_for("users.show", &HashMap::from([("id".into(), "35".into())]))
                .unwrap(),
            "/users/35"
        );
    }
}
