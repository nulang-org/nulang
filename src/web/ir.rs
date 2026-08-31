//! Deployment intermediate representation (IR) for Nulang Web apps.
//!
//! `nula build --web` emits `dist/nulang-app.ir.json`, a JSON document that
//! describes routes, static artifacts, required capabilities, signal graph,
//! budgets, and middleware. Adapters consume this IR to deploy to Nulang Cloud,
//! static hosts, or Docker.

use crate::package::manifest::BudgetsSection;
use crate::runtime::WebRoute;
use crate::web::modules::ModuleRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrRoute {
    pub method: String,
    pub path: String,
    pub placement: String,
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetsIr {
    pub initial_js_max_bytes: Option<usize>,
    pub lcp_seconds: Option<f64>,
}

/// A cloud environment variable or secret required by an imported module.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudConfigEntry {
    pub key: String,
    pub required_by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeploymentIr {
    pub version: u32,
    pub routes: Vec<IrRoute>,
    pub signals: serde_json::Value,
    pub capabilities: Vec<String>,
    pub budgets: BudgetsIr,
    pub middleware: Vec<String>,
    pub cloud_config: Vec<CloudConfigEntry>,
}

impl DeploymentIr {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Generate the deployment IR for a web package.
///
/// `routes` are the routes collected by running the compiled entry point.
/// `signal_graph_path` is the optional path to `app.signals.json` emitted by
/// the reactivity pass. `src_root` is the package `src/` directory used to scan
/// for performed capabilities. `budgets` are parsed from `Nulang.toml`.
pub fn generate_deployment_ir(
    routes: &[WebRoute],
    signal_graph_path: Option<&Path>,
    src_root: &Path,
    budgets: &BudgetsSection,
) -> DeploymentIr {
    let mut ir_routes = Vec::new();
    for route in routes {
        let placement =
            if route.path.contains(':') || !matches!(route.method.as_str(), "GET" | "HEAD") {
                "server".to_string()
            } else {
                "static".to_string()
            };
        let artifact = if placement == "static" {
            Some(route_path_to_artifact(&route.path))
        } else {
            None
        };
        ir_routes.push(IrRoute {
            method: route.method.as_str().to_string(),
            path: route.path.clone(),
            placement,
            artifact,
        });
    }

    let signals = signal_graph_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    let source_text = collect_source_text(src_root);
    let mut capabilities = BTreeSet::new();
    for cap in infer_capabilities(&source_text) {
        capabilities.insert(cap);
    }
    for cap in infer_module_capabilities(&source_text) {
        capabilities.insert(cap);
    }
    let capabilities: Vec<String> = capabilities.into_iter().collect();

    let budgets_ir = BudgetsIr {
        initial_js_max_bytes: budgets.initial_js_max_bytes(),
        lcp_seconds: budgets.lcp_seconds(),
    };

    let cloud_config = infer_module_cloud_config(&source_text);
    let middleware = infer_middleware(&source_text);

    DeploymentIr {
        version: 1,
        routes: ir_routes,
        signals,
        capabilities,
        budgets: budgets_ir,
        middleware,
        cloud_config,
    }
}

fn route_path_to_artifact(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        "index.html".to_string()
    } else {
        format!("{}/index.html", trimmed)
    }
}

/// Concatenate all `.nula` source files under `src_root` into a single string
/// so capability scanning can see effects performed anywhere in the package.
fn collect_source_text(src_root: &Path) -> String {
    let mut out = String::new();
    if !src_root.is_dir() {
        return out;
    }
    let entries = match std::fs::read_dir(src_root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let p = entry.path();
        if p.is_dir() {
            out.push_str(&collect_source_text(&p));
        } else if p.extension().and_then(|e| e.to_str()) == Some("nula") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                out.push_str(&s);
                out.push('\n');
            }
        }
    }
    out
}

fn infer_capabilities(source: &str) -> Vec<String> {
    let mut caps: HashSet<&str> = HashSet::new();
    let pairs = [
        ("DB", "perform DB."),
        ("Net", "perform Net."),
        ("Realtime", "perform Realtime."),
        ("Http", "perform Http."),
        ("Web", "perform Web."),
        ("Actor", "perform Actor."),
        ("Timer", "perform Timer."),
        ("Job", "perform Job."),
        ("IO", "perform IO."),
    ];
    for (cap, needle) in pairs {
        if source.contains(needle) {
            caps.insert(cap);
        }
    }
    let mut caps: Vec<String> = caps.into_iter().map(|s| s.to_string()).collect();
    caps.sort();
    caps
}

/// Infer capabilities contributed by imported `@nulang/*` modules.
fn infer_module_capabilities(source: &str) -> Vec<String> {
    let registry = ModuleRegistry::builtin();
    let imports = collect_nulang_imports(source);
    registry.collect_capabilities(&imports)
}

/// Infer cloud config keys required by imported `@nulang/*` modules.
fn infer_module_cloud_config(source: &str) -> Vec<CloudConfigEntry> {
    let registry = ModuleRegistry::builtin();
    let imports = collect_nulang_imports(source);
    let mut entries = Vec::new();
    for name in imports {
        if let Some(spec) = registry.get(&name) {
            for key in &spec.cloud_config_keys {
                entries.push(CloudConfigEntry {
                    key: key.clone(),
                    required_by: name.clone(),
                });
            }
        }
    }
    entries
}

/// Infer default middleware stack, extending it when imported modules
/// contribute security concerns (e.g., auth sessions).
fn infer_middleware(source: &str) -> Vec<String> {
    let mut stack = vec![
        "security_headers".to_string(),
        "request_log".to_string(),
        "csrf".to_string(),
    ];
    let imports = collect_nulang_imports(source);
    if imports.iter().any(|name| name == "@nulang/auth") {
        stack.push("auth".to_string());
    }
    stack
}

/// Collect all `@nulang/*` import names from the source text.
fn collect_nulang_imports(source: &str) -> Vec<String> {
    let mut imports = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("import ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if name.starts_with("@nulang/") && !imports.contains(&name.to_string()) {
                imports.push(name.to_string());
            }
        }
    }
    imports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_path_to_artifact() {
        assert_eq!(route_path_to_artifact("/"), "index.html");
        assert_eq!(route_path_to_artifact("/about"), "about/index.html");
        assert_eq!(
            route_path_to_artifact("/blog/:slug"),
            "blog/:slug/index.html"
        );
    }

    #[test]
    fn test_infer_capabilities() {
        let src = r#"
            fn foo() {
                perform DB.query("...")
                perform Realtime.broadcast("room", "hi")
            }
        "#;
        let caps = infer_capabilities(src);
        assert!(caps.contains(&"DB".to_string()));
        assert!(caps.contains(&"Realtime".to_string()));
        assert!(!caps.contains(&"Net".to_string()));
    }

    #[test]
    fn test_infer_module_capabilities_from_imports() {
        let src = r#"
            import stdlib::web::html
            import @nulang/auth
            import @nulang/postgres

            fn main() {}
        "#;
        let caps = infer_module_capabilities(src);
        assert!(caps.contains(&"auth".to_string()));
        assert!(caps.contains(&"DB".to_string()));
        assert!(!caps.contains(&"payments".to_string()));
    }

    #[test]
    fn test_infer_module_cloud_config_from_imports() {
        let src = r#"
            import @nulang/auth
            import @nulang/postgres

            fn main() {}
        "#;
        let entries = infer_module_cloud_config(src);
        let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
        assert!(keys.contains(&"AUTH_COOKIE_SECRET".to_string()));
        assert!(keys.contains(&"DATABASE_URL".to_string()));
        assert!(entries
            .iter()
            .any(|e| e.key == "AUTH_COOKIE_SECRET" && e.required_by == "@nulang/auth"));
    }

    #[test]
    fn test_infer_middleware_adds_auth_when_imported() {
        let src = r#"
            import @nulang/auth
            fn main() {}
        "#;
        let stack = infer_middleware(src);
        assert!(stack.contains(&"auth".to_string()));
        assert!(stack.contains(&"csrf".to_string()));

        let src_no_auth = r#"
            import stdlib::web::html
            fn main() {}
        "#;
        let stack_no_auth = infer_middleware(src_no_auth);
        assert!(!stack_no_auth.contains(&"auth".to_string()));
    }
}
