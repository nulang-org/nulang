//! Browser route URL helpers generated from validated Web Contract IR.
//! This is additive interop, not a second router or an authorization boundary.
use crate::web::bindings::{compile_route_bindings, RouteBindingSource};
use crate::web::contracts::ContractCompilation;
use crate::web::runtime_bindings::{compile_runtime_route_plan, RuntimeRouteSegment};
use crate::web::validation::validation_diagnostics;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub struct RouteLinkModules {
    pub javascript: String,
    pub declarations: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LinkSegment {
    Literal { value: String },
    Param { name: String, ty: String },
}

#[derive(Serialize)]
struct LinkSpec {
    segments: Vec<LinkSegment>,
    query: BTreeMap<String, String>,
}

fn ts_type(ty: &str) -> &'static str {
    match ty {
        "Int" | "Float" => "number",
        "Bool" => "boolean",
        _ => "string", // opaque identifiers are string-backed by the Web request binder
    }
}

fn ts_property(name: &str) -> String {
    if name
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        name.to_string()
    } else {
        serde_json::to_string(name).expect("string JSON serialization is infallible")
    }
}

/// Generate opt-in ES module + TypeScript declarations for browser clients.
///
/// The caller must provide the same compiler-validated contract compilation
/// used by dispatch and OpenAPI. Revalidate defensively so this cannot export
/// malformed or ambiguous routes. No browser runtime is automatically loaded.
pub fn generate_route_link_modules(
    contracts: &ContractCompilation,
) -> Result<RouteLinkModules, Vec<String>> {
    let mut diagnostics = validation_diagnostics(contracts);
    let mut specs = BTreeMap::<String, LinkSpec>::new();
    let mut overloads = Vec::new();

    for route in &contracts.routes {
        let key = format!("{} {}", route.method, route.path);
        if specs.contains_key(&key) {
            diagnostics.push(format!("duplicate browser route link {key}"));
            continue;
        }
        let plan = match compile_runtime_route_plan(route) {
            Ok(plan) => plan,
            Err(errors) => {
                diagnostics.extend(errors);
                continue;
            }
        };
        let bindings = compile_route_bindings(route).bindings;
        let mut params = BTreeMap::new();
        let segments = plan
            .segments
            .into_iter()
            .map(|segment| match segment {
                RuntimeRouteSegment::Literal(value) => LinkSegment::Literal { value },
                RuntimeRouteSegment::PathParam(name) => {
                    let ty = route
                        .params
                        .iter()
                        .find(|param| param.name == name)
                        .and_then(|param| param.ty.as_deref())
                        .or_else(|| {
                            bindings
                                .iter()
                                .find(|b| {
                                    b.source == RouteBindingSource::Path && b.source_name == name
                                })
                                .and_then(|b| b.ty.as_deref())
                        })
                        .unwrap_or("String")
                        .to_string();
                    if params.insert(name.clone(), ty.clone()).is_some() {
                        diagnostics.push(format!("{key}: repeats path parameter '{name}'"));
                    }
                    LinkSegment::Param { name, ty }
                }
            })
            .collect();
        let mut query = BTreeMap::new();
        let mut duplicate_queries = BTreeSet::new();
        for binding in &bindings {
            if binding.source == RouteBindingSource::Query {
                if query
                    .insert(
                        binding.source_name.clone(),
                        binding.ty.clone().unwrap_or_else(|| "String".into()),
                    )
                    .is_some()
                {
                    duplicate_queries.insert(binding.source_name.clone());
                }
            }
        }
        for duplicate in duplicate_queries {
            diagnostics.push(format!("{key}: duplicate query binding '{duplicate}'"));
        }
        let literal = serde_json::to_string(&key).expect("string JSON serialization is infallible");
        let mut members = Vec::new();
        if !params.is_empty() {
            let fields = params
                .iter()
                .map(|(k, v)| format!("{}: {}", ts_property(k), ts_type(v)))
                .collect::<Vec<_>>()
                .join("; ");
            members.push(format!("path: {{ {fields} }}"));
        }
        if !query.is_empty() {
            let fields = query
                .iter()
                .map(|(k, v)| format!("{}: {}", ts_property(k), ts_type(v)))
                .collect::<Vec<_>>()
                .join("; ");
            members.push(format!("query: {{ {fields} }}"));
        }
        let args = if members.is_empty() {
            "args?: { path?: never; query?: never }".to_string()
        } else {
            format!("args: {{ {} }}", members.join("; "))
        };
        overloads.push(format!(
            "export declare function urlFor(route: {literal}, {args}): string;"
        ));
        specs.insert(key, LinkSpec { segments, query });
    }

    if !diagnostics.is_empty() {
        diagnostics.sort();
        diagnostics.dedup();
        return Err(diagnostics);
    }

    overloads.sort();
    let spec_json = serde_json::to_string(&specs).expect("route link specs serialize");
    let javascript = format!(
        "// Generated from validated Nulang Web route contracts. Do not edit.\nconst routeSpecs = Object.freeze({spec_json});\n{}",
        include_str!("route_links_runtime.mjs")
    );
    let declarations = format!(
        "// Generated from validated Nulang Web route contracts. Do not edit.\n{}\n",
        if overloads.is_empty() {
            "export declare function urlFor(route: never, args?: never): string;".to_string()
        } else {
            overloads.join("\n")
        }
    );
    Ok(RouteLinkModules {
        javascript,
        declarations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{
        HandlerParamContract, RequestParamBindingContract, RequestParamSource, RouteContract,
        RouteParamContract,
    };

    fn fixture() -> ContractCompilation {
        ContractCompilation {
            routes: vec![RouteContract {
                method: "GET".into(),
                path: "/users/{id: Int}/posts/:slug".into(),
                handler: Some("show_post".into()),
                params: vec![
                    RouteParamContract {
                        name: "id".into(),
                        ty: Some("Int".into()),
                    },
                    RouteParamContract {
                        name: "slug".into(),
                        ty: Some("String".into()),
                    },
                ],
                handler_params: vec![
                    HandlerParamContract {
                        name: "id".into(),
                        ty: Some("Int".into()),
                        capability: None,
                        request: None,
                    },
                    HandlerParamContract {
                        name: "slug".into(),
                        ty: Some("String".into()),
                        capability: None,
                        request: None,
                    },
                    HandlerParamContract {
                        name: "filter".into(),
                        ty: Some("Bool".into()),
                        capability: None,
                        request: Some(RequestParamBindingContract {
                            source: RequestParamSource::Query,
                            source_name: "published".into(),
                        }),
                    },
                ],
                ..RouteContract::default()
            }],
            diagnostics: vec![],
        }
    }

    #[test]
    fn generates_typed_path_and_query_overload() {
        let output = generate_route_link_modules(&fixture()).unwrap();
        assert!(output
            .declarations
            .contains("\"GET /users/{id: Int}/posts/:slug\""));
        assert!(output.declarations.contains("id: number"));
        assert!(output.declarations.contains("slug: string"));
        assert!(output
            .declarations
            .contains("query: { published: boolean }"));
        assert!(output.javascript.contains("export function urlFor"));
    }

    #[test]
    fn runtime_encodes_unicode_and_rejects_invalid_path_or_query_values() {
        use std::process::Command;
        let output = generate_route_link_modules(&fixture()).unwrap();
        // Data URL avoids filesystem temp quotas and works across operating systems.
        let source = serde_json::to_string(&output.javascript).unwrap();
        let script = format!(
            r#"
            const {{ urlFor }} = await import('data:text/javascript,' + encodeURIComponent({source}));
            import assert from 'node:assert/strict';
            const key = 'GET /users/{{id: Int}}/posts/:slug';
            assert.equal(urlFor(key, {{ path: {{ id: 42, slug: 'olá mundo' }}, query: {{ published: false }} }}), '/users/42/posts/ol%C3%A1%20mundo?published=false');
            for (const args of [
                {{ path: {{ id: 42, slug: '../admin' }} }},
                {{ path: {{ id: 42, slug: 'a/b' }} }},
                {{ path: {{ id: '42', slug: 'ok' }} }},
                {{ path: {{ id: 42 }} }},
                {{ path: {{ id: 42, slug: 'ok', extra: 'x' }} }},
                {{ path: {{ id: 42, slug: 'ok' }}, query: {{ unknown: 'x' }} }},
                {{ path: {{ id: 42, slug: 'ok' }}, query: {{ published: 'false' }} }},
                {{ path: {{ id: 42, slug: 'ok' }} }},
            ]) assert.throws(() => urlFor(key, args));
            assert.throws(() => urlFor('__proto__', {{}}));
        "#,
            source = source
        );
        let result = Command::new("node")
            .args(["--input-type=module", "--eval", &script])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn static_route_requires_no_arguments_and_preserves_path() {
        let input = ContractCompilation {
            routes: vec![RouteContract {
                method: "GET".into(),
                path: "/about".into(),
                ..RouteContract::default()
            }],
            diagnostics: vec![],
        };
        let output = generate_route_link_modules(&input).unwrap();
        assert!(output
            .declarations
            .contains("args?: { path?: never; query?: never }"));
        assert!(output.javascript.contains("\"GET /about\""));
    }

    #[test]
    fn rejects_repeated_path_capture_names() {
        let mut input = fixture();
        input.routes[0].path = "/users/{id: Int}/{id: Int}".into();
        input.routes[0].params = vec![
            RouteParamContract {
                name: "id".into(),
                ty: Some("Int".into()),
            },
            RouteParamContract {
                name: "id".into(),
                ty: Some("Int".into()),
            },
        ];
        input.routes[0].handler_params.truncate(1);
        let errors = generate_route_link_modules(&input).err().unwrap();
        assert!(
            errors.iter().any(|e| e.contains("repeats path parameter")),
            "{errors:?}"
        );
    }

    #[test]
    fn rejects_conflicting_query_bindings() {
        let mut input = fixture();
        let copy = input.routes[0].handler_params[2].clone();
        input.routes[0].handler_params.push(copy);
        assert!(generate_route_link_modules(&input).is_err());
    }
}
