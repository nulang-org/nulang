//! Parsing of `Nulang.toml` package manifests.
//!
//! A manifest looks like:
//!
//! ```toml
//! [package]
//! name = "my-app"
//! version = "0.1.0"
//! entry = "src/main.nula"   # optional; this is the default
//!
//! [dependencies]
//! util = { path = "../util" }
//! json = { git = "https://github.com/example/json.nu.git", tag = "v0.2.0" }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::{NuError, NuResult, Span};

/// Manifest file name, expected at the root of every package.
pub const MANIFEST_FILE: &str = "Nulang.toml";

/// Default entry point, relative to the package root.
pub const DEFAULT_ENTRY: &str = "src/main.nula";

/// A parsed `Nulang.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Manifest {
    pub package: PackageSection,
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    pub web: WebSection,
    #[serde(default)]
    pub budgets: BudgetsSection,
}

impl Default for WebSection {
    fn default() -> Self {
        Self {
            port: default_web_port(),
            static_dir: default_static_dir(),
            output_dir: default_output_dir(),
        }
    }
}

/// The `[package]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PackageSection {
    pub name: String,
    pub version: String,
    /// Entry point relative to the package root; `src/main.nula` when omitted.
    #[serde(default = "default_entry")]
    pub entry: String,
    /// Registry URL for publishing and fetching dependencies.
    /// When set, `nula publish` uploads here and bare version deps resolve from here.
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// Resource capabilities the package needs (e.g. `["net"]` for the
    /// `Http` effect). Forwarded to the compiler as `--with <cap>` by
    /// `nula build`, `nula test`, and `nula run`, so packages performing
    /// gated effects (Net, ...) can declare their requirements instead of
    /// failing the default-deny capability check.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

fn default_entry() -> String {
    DEFAULT_ENTRY.to_string()
}

/// One entry in `[dependencies]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Dependency {
    /// `foo = "0.1.0"` — a bare version requirement. Resolved from the
    /// package's configured registry (`[package] registry`) at build time.
    Version(String),
    /// `foo = { path = "../foo" }` or `foo = { git = "...", ... }`.
    Detailed(DependencyDetail),
}

/// The `[web]` section of a Nulang Web manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WebSection {
    /// Port for the `nula dev` server. Defaults to 8080.
    #[serde(default = "default_web_port")]
    pub port: u16,
    /// Directory with static assets, relative to the package root.
    #[serde(default = "default_static_dir")]
    pub static_dir: String,
    /// Directory where `nula build --web` emits the static site.
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
}

fn default_web_port() -> u16 {
    8080
}

fn default_static_dir() -> String {
    "static".to_string()
}

fn default_output_dir() -> String {
    "dist".to_string()
}

/// The `[budgets]` section of a Nulang Web manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct BudgetsSection {
    /// Maximum initial JavaScript transfer size, e.g. "20KB" or "1.5MB".
    #[serde(default)]
    pub initial_js: Option<String>,
    /// Largest Contentful Paint target, e.g. "1.5s".
    #[serde(default)]
    pub lcp: Option<String>,
}

impl BudgetsSection {
    /// Parse `initial_js` into a byte budget if declared.
    pub fn initial_js_max_bytes(&self) -> Option<usize> {
        self.initial_js.as_ref().and_then(|s| parse_size(s))
    }

    /// Parse `lcp` into a seconds target if declared.
    pub fn lcp_seconds(&self) -> Option<f64> {
        self.lcp.as_ref().and_then(|s| {
            s.trim()
                .split('s')
                .next()
                .map(|v| v.trim())
                .and_then(|v| v.parse().ok())
        })
    }
}

fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim().replace(' ', "");
    let num_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let num: f64 = s[..num_end].parse().ok()?;
    let unit = s[num_end..].trim().to_uppercase();
    let mult = match unit.as_str() {
        "" | "B" => 1.0,
        "KB" | "K" => 1024.0,
        "MB" | "M" => 1024.0 * 1024.0,
        "GB" | "G" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num * mult) as usize)
}

/// Table form of a dependency: a local path, a git URL, or both refined by a
/// version requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct DependencyDetail {
    pub path: Option<String>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub version: Option<String>,
}

impl Manifest {
    /// Parse a manifest from its TOML text.
    pub fn parse(source: &str) -> NuResult<Manifest> {
        toml::from_str(source).map_err(|e| NuError::PackageError {
            msg: format!("invalid {}: {}", MANIFEST_FILE, e),
            span: Span::default(),
        })
    }

    /// Load and parse the manifest in `dir`.
    pub fn load(dir: &Path) -> NuResult<Manifest> {
        let path = dir.join(MANIFEST_FILE);
        let source = std::fs::read_to_string(&path).map_err(|e| NuError::PackageError {
            msg: format!("cannot read {}: {}", path.display(), e),
            span: Span::default(),
        })?;
        Self::parse(&source)
    }

    /// Serialize this manifest to a TOML string.
    pub fn to_toml(&self) -> NuResult<String> {
        toml::to_string_pretty(self).map_err(|e| NuError::PackageError {
            msg: format!("cannot serialize {}: {}", MANIFEST_FILE, e),
            span: Span::default(),
        })
    }

    /// Write this manifest into `dir`.
    pub fn save(&self, dir: &Path) -> NuResult<()> {
        let path = dir.join(MANIFEST_FILE);
        std::fs::write(&path, self.to_toml()?).map_err(|e| NuError::PackageError {
            msg: format!("cannot write {}: {}", path.display(), e),
            span: Span::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_parse_minimal() {
        let source = r#"
            [package]
            name = "my-app"
            version = "0.1.0"
        "#;
        let manifest = Manifest::parse(source).expect("minimal manifest should parse");
        assert_eq!(manifest.package.name, "my-app");
        assert_eq!(manifest.package.version, "0.1.0");
        assert_eq!(manifest.package.entry, DEFAULT_ENTRY);
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn test_manifest_parse_with_dependencies() {
        let source = r#"
            [package]
            name = "my-app"
            version = "0.2.0"
            entry = "src/app.nula"

            [dependencies]
            util = { path = "../util" }
            json = { git = "https://github.com/example/json.nu.git", tag = "v0.2.0" }
            fancy = { git = "https://example.com/fancy.git", rev = "abc123", version = "1.0.0" }
            registry_dep = "0.3.0"
        "#;
        let manifest = Manifest::parse(source).expect("manifest with deps should parse");
        assert_eq!(manifest.package.entry, "src/app.nula");
        assert_eq!(manifest.dependencies.len(), 4);

        let util = &manifest.dependencies["util"];
        assert_eq!(
            *util,
            Dependency::Detailed(DependencyDetail {
                path: Some("../util".to_string()),
                ..Default::default()
            })
        );

        let json = &manifest.dependencies["json"];
        match json {
            Dependency::Detailed(d) => {
                assert_eq!(
                    d.git.as_deref(),
                    Some("https://github.com/example/json.nu.git")
                );
                assert_eq!(d.tag.as_deref(), Some("v0.2.0"));
                assert_eq!(d.path, None);
            }
            Dependency::Version(_) => panic!("json should be a detailed dependency"),
        }

        assert_eq!(
            manifest.dependencies["registry_dep"],
            Dependency::Version("0.3.0".to_string())
        );
    }

    #[test]
    fn test_manifest_parse_missing_name_fails() {
        let source = r#"
            [package]
            version = "0.1.0"
        "#;
        let err = Manifest::parse(source).expect_err("name is required");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains(MANIFEST_FILE)),
            other => panic!("expected PackageError, got {:?}", other),
        }
    }

    #[test]
    fn test_manifest_parse_invalid_toml_fails() {
        let err = Manifest::parse("not [valid toml").expect_err("garbage should not parse");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));
    }

    #[test]
    fn test_manifest_parse_web_defaults() {
        let source = r#"
            [package]
            name = "hello-web"
            version = "0.1.0"
        "#;
        let manifest = Manifest::parse(source).expect("manifest should parse");
        assert_eq!(manifest.web.port, 8080);
        assert_eq!(manifest.web.static_dir, "static");
        assert_eq!(manifest.web.output_dir, "dist");
    }

    #[test]
    fn test_manifest_parse_web_overrides() {
        let source = r#"
            [package]
            name = "hello-web"
            version = "0.1.0"

            [web]
            port = 3000
            static_dir = "public"
            output_dir = "site"
        "#;
        let manifest = Manifest::parse(source).expect("manifest should parse");
        assert_eq!(manifest.web.port, 3000);
        assert_eq!(manifest.web.static_dir, "public");
        assert_eq!(manifest.web.output_dir, "site");
    }

    #[test]
    fn test_manifest_parse_budgets() {
        let source = r#"
            [package]
            name = "hello-web"
            version = "0.1.0"

            [budgets]
            initial_js = "20KB"
            lcp = "1.5s"
        "#;
        let manifest = Manifest::parse(source).expect("manifest should parse");
        assert_eq!(manifest.budgets.initial_js_max_bytes(), Some(20 * 1024));
        assert_eq!(manifest.budgets.lcp_seconds(), Some(1.5));
    }

    #[test]
    fn test_manifest_parse_budgets_default() {
        let source = r#"
            [package]
            name = "hello-web"
            version = "0.1.0"
        "#;
        let manifest = Manifest::parse(source).expect("manifest should parse");
        assert_eq!(manifest.budgets.initial_js_max_bytes(), None);
        assert_eq!(manifest.budgets.lcp_seconds(), None);
    }
}
