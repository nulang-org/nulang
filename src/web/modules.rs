//! Module registry for Nulang Web packages (`@nulang/*`).
//!
//! Each module can declare runtime capabilities, CLI subcommands, and Cloud
//! config fragments that the compiler and package manager consult. This is the
//! built-in registry of first-party modules; in the future each module will
//! ship a `nulang-module.json` that is loaded from the resolved package source.

use std::collections::BTreeMap;

/// Metadata describing a registered `@nulang/*` module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSpec {
    /// Runtime capabilities this module registers (e.g., "auth", "postgres").
    pub capabilities: Vec<String>,
    /// CLI subcommands contributed by the module (e.g., "auth enable").
    pub cli_subcommands: Vec<String>,
    /// Cloud config keys required by the module (e.g., "AUTH_COOKIE_SECRET").
    pub cloud_config_keys: Vec<String>,
    /// Optional compiler desugar pass registered by this module. The string is
    /// a pass name the compiler looks up in its built-in desugar pass table.
    pub desugar_pass: Option<String>,
}

/// Registry of known modules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleRegistry {
    pub modules: BTreeMap<String, ModuleSpec>,
}

impl ModuleRegistry {
    /// Built-in registry of first-party `@nulang/*` modules.
    pub fn builtin() -> Self {
        let mut modules = BTreeMap::new();
        modules.insert(
            "@nulang/web".to_string(),
            ModuleSpec {
                capabilities: vec![
                    "Render".to_string(),
                    "Request".to_string(),
                    "Realtime".to_string(),
                ],
                cli_subcommands: vec![],
                cloud_config_keys: vec![],
                desugar_pass: None,
            },
        );
        modules.insert(
            "@nulang/auth".to_string(),
            ModuleSpec {
                capabilities: vec!["auth".to_string()],
                cli_subcommands: vec!["auth enable".to_string()],
                cloud_config_keys: vec!["AUTH_COOKIE_SECRET".to_string()],
                desugar_pass: None,
            },
        );
        modules.insert(
            "@nulang/postgres".to_string(),
            ModuleSpec {
                capabilities: vec!["DB".to_string()],
                cli_subcommands: vec![],
                cloud_config_keys: vec!["DATABASE_URL".to_string()],
                desugar_pass: None,
            },
        );
        modules.insert(
            "@nulang/stripe".to_string(),
            ModuleSpec {
                capabilities: vec!["payments".to_string()],
                cli_subcommands: vec![],
                cloud_config_keys: vec!["STRIPE_API_KEY".to_string()],
                desugar_pass: None,
            },
        );
        modules.insert(
            "@nulang/ai".to_string(),
            ModuleSpec {
                capabilities: vec!["Inference".to_string()],
                cli_subcommands: vec![],
                cloud_config_keys: vec!["LLM_API_KEY".to_string()],
                desugar_pass: None,
            },
        );
        Self { modules }
    }

    /// Look up a module by import name.
    pub fn get(&self, name: &str) -> Option<&ModuleSpec> {
        self.modules.get(name)
    }

    /// All capabilities registered by all modules, deduplicated and sorted.
    pub fn all_capabilities(&self) -> Vec<String> {
        self.collect_capabilities(self.modules.keys().cloned().collect::<Vec<_>>().as_slice())
    }

    /// Capabilities contributed by a specific set of imported module names.
    pub fn collect_capabilities(&self, imports: &[String]) -> Vec<String> {
        let mut caps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for name in imports {
            if let Some(spec) = self.modules.get(name) {
                for cap in &spec.capabilities {
                    caps.insert(cap.clone());
                }
            }
        }
        caps.into_iter().collect()
    }

    /// Cloud config keys required by a specific set of imported module names.
    pub fn collect_cloud_config_keys(&self, imports: &[String]) -> Vec<String> {
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for name in imports {
            if let Some(spec) = self.modules.get(name) {
                for key in &spec.cloud_config_keys {
                    keys.insert(key.clone());
                }
            }
        }
        keys.into_iter().collect()
    }

    /// All cloud config keys registered by all modules, deduplicated and sorted.
    pub fn all_cloud_config_keys(&self) -> Vec<String> {
        self.collect_cloud_config_keys(self.modules.keys().cloned().collect::<Vec<_>>().as_slice())
    }

    /// Return the module name that corresponds to a locked dependency name.
    ///
    /// First-party packages are named `nulang-<name>` in the lockfile and map
    /// to `@nulang/<name>` for import and registry lookup.
    pub fn module_name_for_dep(dep_name: &str) -> Option<String> {
        if dep_name.starts_with("nulang-") {
            Some(format!("@nulang/{}", &dep_name["nulang-".len()..]))
        } else if dep_name.starts_with("@nulang/") {
            Some(dep_name.to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_registry_has_core_modules() {
        let reg = ModuleRegistry::builtin();
        assert!(reg.get("@nulang/web").is_some());
        assert!(reg.get("@nulang/auth").is_some());
        assert!(reg.get("@nulang/postgres").is_some());
    }

    #[test]
    fn test_all_capabilities_includes_auth_and_db() {
        let reg = ModuleRegistry::builtin();
        let caps = reg.all_capabilities();
        assert!(caps.contains(&"auth".to_string()));
        assert!(caps.contains(&"DB".to_string()));
        assert!(caps.contains(&"Render".to_string()));
    }

    #[test]
    fn test_collect_capabilities_for_imports() {
        let reg = ModuleRegistry::builtin();
        let caps =
            reg.collect_capabilities(&["@nulang/auth".to_string(), "@nulang/postgres".to_string()]);
        assert!(caps.contains(&"auth".to_string()));
        assert!(caps.contains(&"DB".to_string()));
        assert!(!caps.contains(&"payments".to_string()));
    }

    #[test]
    fn test_collect_cloud_config_keys_for_imports() {
        let reg = ModuleRegistry::builtin();
        let keys = reg.collect_cloud_config_keys(&[
            "@nulang/auth".to_string(),
            "@nulang/postgres".to_string(),
        ]);
        assert!(keys.contains(&"AUTH_COOKIE_SECRET".to_string()));
        assert!(keys.contains(&"DATABASE_URL".to_string()));
    }

    #[test]
    fn test_module_name_for_dep_maps_first_party() {
        assert_eq!(
            ModuleRegistry::module_name_for_dep("nulang-auth"),
            Some("@nulang/auth".to_string())
        );
        assert_eq!(
            ModuleRegistry::module_name_for_dep("@nulang/ai"),
            Some("@nulang/ai".to_string())
        );
        assert_eq!(ModuleRegistry::module_name_for_dep("third-party"), None);
    }
}
