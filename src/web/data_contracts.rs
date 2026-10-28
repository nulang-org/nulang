//! Compiler-validated Web loader and mutation dependency metadata.
//! Dependency names are explicit identities, never inferred storage reads.
use crate::package::manifest::WebSection;
use crate::web::contracts::ContractCompilation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WebDataContracts {
    pub loaders: Vec<LoaderContract>,
    pub mutations: Vec<MutationContract>,
}

impl WebDataContracts {
    pub fn is_empty(&self) -> bool {
        self.loaders.is_empty() && self.mutations.is_empty()
    }

    /// Append only validated compile-time metadata to the existing generated
    /// client script. No additional request or runtime dependency is needed.
    pub fn append_to_client_script(&self, path: &std::path::Path) -> std::io::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        use std::io::Write;
        let payload = serde_json::to_string(self).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        write!(file, "\n;window.nulangCompiledData = {payload};\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoaderContract {
    pub id: String,
    pub path: String,
    pub target: String,
    pub reads: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationContract {
    pub path: String,
    pub invalidates: Vec<String>,
    pub targets: Vec<String>,
}

pub fn compile_web_data_contracts(
    web: &WebSection,
    contracts: &ContractCompilation,
) -> Result<WebDataContracts, Vec<String>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut errors = Vec::new();
    let mut loaders = Vec::new();
    let mut ids = BTreeSet::new();
    let mut route_targets = BTreeSet::new();
    let routes: BTreeMap<(&str, &str), _> = contracts
        .routes
        .iter()
        .map(|route| ((route.method.as_str(), route.path.as_str()), route))
        .collect();

    for loader in &web.loaders {
        if !safe_name(&loader.id) {
            errors.push(format!("invalid Web loader id {:?}", loader.id));
        }
        if !ids.insert(loader.id.clone()) {
            errors.push(format!("duplicate Web loader id {:?}", loader.id));
        }
        if !safe_target(&loader.target) {
            errors.push(format!("invalid Web loader target {:?}", loader.target));
        }
        if !route_targets.insert((loader.path.clone(), loader.target.clone())) {
            errors.push(format!(
                "duplicate Web loader target {:?} on {}",
                loader.target, loader.path
            ));
        }
        if !safe_route_path(&loader.path) {
            errors.push(format!("invalid Web loader GET path {:?}", loader.path));
        }
        match routes.get(&("GET", loader.path.as_str())) {
            Some(route)
                if route.response_type.as_deref() == Some("Html") && route.handler.is_some() => {}
            Some(_) => errors.push(format!(
                "Web loader {} requires a typed Html handler on GET {}",
                loader.id, loader.path
            )),
            None => errors.push(format!(
                "Web loader {} requires a registered GET {} route",
                loader.id, loader.path
            )),
        }
        if loader.reads.is_empty() {
            errors.push(format!(
                "Web loader {} must declare read dependencies",
                loader.id
            ));
        }
        let mut reads = BTreeSet::new();
        for dependency in &loader.reads {
            if !safe_dependency(dependency) {
                errors.push(format!("invalid Web loader dependency {:?}", dependency));
            }
            if !reads.insert(dependency.clone()) {
                errors.push(format!(
                    "duplicate read dependency {:?} for loader {}",
                    dependency, loader.id
                ));
            }
        }
        loaders.push(LoaderContract {
            id: loader.id.clone(),
            path: loader.path.clone(),
            target: loader.target.clone(),
            reads: reads.into_iter().collect(),
        });
    }
    loaders.sort_by(|a, b| (&a.id, &a.path).cmp(&(&b.id, &b.path)));

    let mut mutations = Vec::new();
    let mut paths = BTreeSet::new();
    for mutation in &web.mutations {
        if !paths.insert(mutation.path.clone()) {
            errors.push(format!("duplicate Web mutation POST {}", mutation.path));
        }
        if !safe_route_path(&mutation.path) {
            errors.push(format!(
                "invalid Web mutation POST path {:?}",
                mutation.path
            ));
        }
        match routes.get(&("POST", mutation.path.as_str())) {
            Some(route)
                if route.handler.is_some() && route.response_type.as_deref() == Some("Html") => {}
            Some(_) => errors.push(format!(
                "Web mutation POST {} requires a typed Html handler",
                mutation.path
            )),
            None => errors.push(format!(
                "Web mutation requires a registered POST {} route",
                mutation.path
            )),
        }
        if mutation.invalidates.is_empty() {
            errors.push(format!(
                "POST {} must declare invalidated dependencies",
                mutation.path
            ));
        }
        let mut invalidates = BTreeSet::new();
        for dependency in &mutation.invalidates {
            if !safe_dependency(dependency) {
                errors.push(format!("invalid Web mutation dependency {:?}", dependency));
            }
            if !invalidates.insert(dependency.clone()) {
                errors.push(format!(
                    "duplicate invalidation dependency {:?} for POST {}",
                    dependency, mutation.path
                ));
            }
        }
        let route_loaders: Vec<_> = loaders
            .iter()
            .filter(|loader| loader.path == mutation.path)
            .collect();
        let known: BTreeSet<&str> = route_loaders
            .iter()
            .flat_map(|loader| loader.reads.iter().map(String::as_str))
            .collect();
        for dependency in &invalidates {
            if !known.contains(dependency.as_str()) {
                errors.push(format!(
                    "POST {} invalidates undeclared dependency {:?}",
                    mutation.path, dependency
                ));
            }
        }
        let targets: BTreeSet<_> = route_loaders
            .iter()
            .filter(|loader| loader.reads.iter().any(|read| invalidates.contains(read)))
            .map(|loader| loader.target.clone())
            .collect();
        if targets.is_empty() {
            errors.push(format!(
                "POST {} has no matching GET loader targets",
                mutation.path
            ));
        }
        if targets.len() > 8 {
            errors.push(format!(
                "POST {} revalidates more than eight DOM targets",
                mutation.path
            ));
        }
        mutations.push(MutationContract {
            path: mutation.path.clone(),
            invalidates: invalidates.into_iter().collect(),
            targets: targets.into_iter().collect(),
        });
    }
    mutations.sort_by(|a, b| a.path.cmp(&b.path));
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        Ok(WebDataContracts { loaders, mutations })
    } else {
        Err(errors)
    }
}

/// Narrow ASCII identities prevent ambiguity in HTML IDs and JSON consumers.
fn safe_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn safe_target(target: &str) -> bool {
    target.strip_prefix('#').is_some_and(safe_name)
}

fn safe_dependency(dependency: &str) -> bool {
    (1..=64).contains(&dependency.len())
        && dependency
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
        && dependency
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        && !dependency.starts_with("__")
}

/// First version supports exact root-relative route paths, not templates or
/// query/hash patterns; dynamic route bindings need request-scope provenance.
fn safe_route_path(path: &str) -> bool {
    (1..=2048).contains(&path.len())
        && path.starts_with('/')
        && !path.contains("//")
        && !path.split('/').any(|part| part == "." || part == "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::manifest::{WebLoaderSection, WebMutationSection};
    use crate::web::contracts::RouteContract;

    fn contracts() -> ContractCompilation {
        ContractCompilation {
            routes: vec![
                RouteContract {
                    method: "GET".into(),
                    path: "/tasks".into(),
                    handler: Some("show_tasks".into()),
                    response_type: Some("Html".into()),
                    ..Default::default()
                },
                RouteContract {
                    method: "POST".into(),
                    path: "/tasks".into(),
                    handler: Some("save_task".into()),
                    response_type: Some("Html".into()),
                    ..Default::default()
                },
                RouteContract {
                    method: "GET".into(),
                    path: "/other".into(),
                    handler: Some("show_other".into()),
                    response_type: Some("Html".into()),
                    ..Default::default()
                },
            ],
            diagnostics: vec![],
        }
    }

    fn web() -> WebSection {
        WebSection {
            loaders: vec![
                WebLoaderSection {
                    id: "summary".into(),
                    path: "/tasks".into(),
                    target: "#summary".into(),
                    reads: vec!["tasks".into()],
                },
                WebLoaderSection {
                    id: "alerts".into(),
                    path: "/tasks".into(),
                    target: "#alerts".into(),
                    reads: vec!["notifications".into(), "tasks".into()],
                },
                WebLoaderSection {
                    id: "other".into(),
                    path: "/other".into(),
                    target: "#other".into(),
                    reads: vec!["other".into()],
                },
            ],
            mutations: vec![WebMutationSection {
                path: "/tasks".into(),
                invalidates: vec!["tasks".into()],
            }],
            ..WebSection::default()
        }
    }

    #[test]
    fn typed_web_example_extracts_html_handlers_from_real_package() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/revalidation-web/src");
        let compiled = crate::web::validation::compile_validated_contracts_from_tree(&root)
            .expect("the runnable example must produce valid typed route contracts");
        for (method, name) in [("GET", "home"), ("POST", "save_message")] {
            let route = compiled
                .routes
                .iter()
                .find(|route| route.method == method && route.path == "/")
                .expect("expected route");
            assert_eq!(route.handler.as_deref(), Some(name), "{route:?}");
            assert_eq!(route.response_type.as_deref(), Some("Html"), "{route:?}");
        }
    }

    #[test]
    fn resolves_only_matching_dependencies_and_emits_stable_order() {
        let result = compile_web_data_contracts(&web(), &contracts()).unwrap();
        assert_eq!(result.loaders.len(), 3);
        assert_eq!(result.loaders[0].id, "alerts");
        assert_eq!(result.loaders[1].id, "other");
        assert_eq!(result.loaders[2].id, "summary");
        assert_eq!(
            result.mutations,
            vec![MutationContract {
                path: "/tasks".into(),
                invalidates: vec!["tasks".into()],
                targets: vec!["#alerts".into(), "#summary".into()],
            }]
        );
    }

    #[test]
    fn compiled_mutation_metadata_appends_to_existing_runtime_script() {
        let compiled = compile_web_data_contracts(&web(), &contracts()).unwrap();
        let path = std::env::temp_dir().join(format!(
            "nulang-web-data-contracts-{}-script.js",
            std::process::id()
        ));
        std::fs::write(&path, "(function() { window.nulang = {}; })();\n").unwrap();
        compiled.append_to_client_script(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(content.starts_with("(function()"));
        let literal = content
            .split("window.nulangCompiledData = ")
            .nth(1)
            .unwrap()
            .trim_end_matches(";\n");
        let restored: WebDataContracts = serde_json::from_str(literal).unwrap();
        assert_eq!(restored, compiled);
    }

    #[test]
    fn empty_config_preserves_legacy_contracts() {
        let result = compile_web_data_contracts(&WebSection::default(), &contracts()).unwrap();
        assert_eq!(result, WebDataContracts::default());
    }

    #[test]
    fn rejects_nonexistent_or_untyped_loader_get_routes() {
        let mut config = web();
        config.loaders[0].path = "/missing".into();
        assert!(compile_web_data_contracts(&config, &contracts())
            .unwrap_err()
            .join("\n")
            .contains("GET /missing"));

        let mut contracts = contracts();
        contracts.routes[0].response_type = Some("Int".into());
        let errors = compile_web_data_contracts(&web(), &contracts).unwrap_err();
        assert!(errors.join("\n").contains("Html"));
    }

    #[test]
    fn rejects_invalid_and_duplicate_loader_id_and_target() {
        let mut w = web();
        w.loaders[0].id = "alerts".into();
        assert!(compile_web_data_contracts(&w, &contracts()).is_err());
        let mut w = web();
        w.loaders[0].target = "#alerts".into();
        assert!(compile_web_data_contracts(&w, &contracts()).is_err());
        let mut w = web();
        w.loaders[0].target = "body > main".into();
        assert!(compile_web_data_contracts(&w, &contracts()).is_err());
    }

    #[test]
    fn rejects_unknown_and_unrouted_mutation_dependencies() {
        let mut w = web();
        w.mutations[0].invalidates = vec!["not-declared".into()];
        assert!(compile_web_data_contracts(&w, &contracts())
            .unwrap_err()
            .join("\n")
            .contains("not-declared"));
        let mut w = web();
        w.mutations[0].path = "/wrong".into();
        assert!(compile_web_data_contracts(&w, &contracts())
            .unwrap_err()
            .join("\n")
            .contains("POST /wrong"));
    }

    #[test]
    fn rejects_non_matching_loader_routes_and_unbounded_targets() {
        let mut w = web();
        w.loaders[0].path = "/other".into();
        // A target may exist on another route, but invalidations must only
        // affect loaders of the mutation's canonical GET route.
        let result = compile_web_data_contracts(&w, &contracts()).unwrap();
        assert_eq!(result.mutations[0].targets, vec!["#alerts".to_string()]);

        let mut w = web();
        w.loaders = (0..9)
            .map(|n| WebLoaderSection {
                id: format!("region{n}"),
                path: "/tasks".into(),
                target: format!("#region{n}"),
                reads: vec!["tasks".into()],
            })
            .collect();
        assert!(compile_web_data_contracts(&w, &contracts())
            .unwrap_err()
            .join("\n")
            .contains("eight"));
    }

    #[test]
    fn rejects_empty_or_unsafe_dependency_identity() {
        for dependency in ["", "two words", "owner/../../etc", "__proto__"] {
            let mut w = web();
            w.loaders[0].reads = vec![dependency.into()];
            assert!(
                compile_web_data_contracts(&w, &contracts()).is_err(),
                "{dependency:?}"
            );
        }
    }
}
