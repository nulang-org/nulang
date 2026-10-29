//! Read-only Web Contract IR preflight for CI and coding agents.
//!
//! This intentionally does not compile, execute or deploy application code.
//! Its scope is the existing compiler-owned route extraction and binding
//! validator, not a production-security audit.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WebCheckReport {
    pub schema_version: u32,
    pub ok: bool,
    pub route_count: usize,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebCheckOptions {
    pub json: bool,
    pub root: PathBuf,
}

/// Accept at most one package root; reject unknown flags rather than
/// silently treating misspelled settings as paths.
pub fn parse_check_args(args: &[String]) -> Result<WebCheckOptions, String> {
    let mut json = false;
    let mut root: Option<PathBuf> = None;
    for arg in args {
        match arg.as_str() {
            "--json" if !json => json = true,
            "--json" => return Err("duplicate --json flag".to_string()),
            flag if flag.starts_with('-') => {
                return Err(format!("unknown web check option: {flag}"));
            }
            directory if root.is_none() => root = Some(PathBuf::from(directory)),
            _ => return Err("web check accepts only one package directory".to_string()),
        }
    }
    Ok(WebCheckOptions {
        json,
        root: root.unwrap_or_else(|| PathBuf::from(".")),
    })
}

/// Validate route extraction and typed request binding against one package's
/// src tree. The same diagnostic aggregator is used by the web build path.
pub fn check_web_package(root: &Path) -> WebCheckReport {
    let src_root = root.join("src");
    if !src_root.is_dir() {
        return WebCheckReport {
            schema_version: 1,
            ok: false,
            route_count: 0,
            diagnostics: vec![format!(
                "{}: source directory does not exist or is not a directory",
                src_root.display()
            )],
        };
    }
    let compilation = crate::web::package_contracts::compile_contracts_from_tree(&src_root);
    let diagnostics = crate::web::validation::validation_diagnostics(&compilation);
    WebCheckReport {
        schema_version: 1,
        ok: diagnostics.is_empty(),
        route_count: compilation.routes.len(),
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(source: &str) -> (std::path::PathBuf, impl Drop) {
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "nulang-web-preflight-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.nula"), source).unwrap();
        (root.clone(), Cleanup(root))
    }

    #[test]
    fn reports_valid_routes_without_running_the_application() {
        let (root, _cleanup) = fixture(
            r#"
            fn home() -> String { "hello" }
            fn web_main() { perform Web.route("GET", "/", home) }
        "#,
        );
        let report = check_web_package(&root);
        assert!(report.ok, "{:?}", report.diagnostics);
        assert_eq!(report.route_count, 1);
        assert_eq!(report.schema_version, 1);
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn invalid_typed_route_is_nonzero_and_machine_readable() {
        let (root, _cleanup) = fixture(
            r#"
            fn home() -> String { "hello" }
            fn web_main() { perform Web.route("GET", "/users/{id: Int}", home) }
        "#,
        );
        let report = check_web_package(&root);
        assert!(!report.ok);
        assert_eq!(report.route_count, 1);
        assert!(report.diagnostics.iter().any(|d| d.contains("id")));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["ok"], false);
        assert!(json["diagnostics"].is_array());
    }

    #[test]
    fn missing_source_directory_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "nulang-web-preflight-absent-{}",
            std::process::id()
        ));
        let report = check_web_package(&root);
        assert!(!report.ok);
        assert_eq!(report.route_count, 0);
        assert_eq!(report.diagnostics.len(), 1);
        assert!(report.diagnostics[0].contains("source directory"));
    }

    #[test]
    fn cli_rejects_ambiguous_arguments() {
        assert!(parse_check_args(&["--json".into(), "project".into()]).is_ok());
        assert!(parse_check_args(&["a".into(), "b".into()]).is_err());
        assert!(parse_check_args(&["--unsafe".into()]).is_err());
        assert!(parse_check_args(&["--json".into(), "--json".into()]).is_err());
    }
}
