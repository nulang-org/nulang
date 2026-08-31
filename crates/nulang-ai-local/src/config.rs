//! Agent runtime configuration (`agent.toml`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigFile {
    #[serde(default)]
    pub inference: InferenceSection,
    #[serde(default)]
    pub director: DirectorSection,
    #[serde(default)]
    pub storage: StorageSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceSection {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectorSection {
    #[serde(default = "default_budget")]
    pub default_budget_usd: f64,
    #[serde(default = "default_managers")]
    pub managers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

impl Default for AgentConfigFile {
    fn default() -> Self {
        Self {
            inference: InferenceSection::default(),
            director: DirectorSection::default(),
            storage: StorageSection::default(),
        }
    }
}

impl Default for InferenceSection {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            provider: default_provider(),
            model: default_model(),
            base_url: None,
        }
    }
}

impl Default for DirectorSection {
    fn default() -> Self {
        Self {
            default_budget_usd: default_budget(),
            managers: default_managers(),
        }
    }
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

fn default_mode() -> String {
    "direct".into()
}
fn default_provider() -> String {
    "openai".into()
}
fn default_model() -> String {
    "gpt-4o".into()
}
fn default_budget() -> f64 {
    25.0
}
fn default_managers() -> Vec<String> {
    vec!["engineering".into()]
}
fn default_data_dir() -> String {
    "~/.nulang/agents".into()
}

impl AgentConfigFile {
    pub fn template() -> &'static str {
        r#"[inference]
mode = "direct"
provider = "openai"
model = "gpt-4o"

[director]
default_budget_usd = 25.0
managers = ["engineering"]

[storage]
data_dir = "~/.nulang/agents"
"#
    }

    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        let path = dir.join("agent.toml");
        let raw = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        toml::from_str(&raw).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            message: e.to_string(),
        })
    }

    pub fn write_init(dir: &Path) -> Result<(), ConfigError> {
        std::fs::create_dir_all(dir).map_err(|e| ConfigError::Io {
            path: dir.display().to_string(),
            source: e,
        })?;
        let path = dir.join("agent.toml");
        if path.exists() {
            return Err(ConfigError::Exists(path.display().to_string()));
        }
        std::fs::write(&path, Self::template()).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        Ok(())
    }

    pub fn resolve_data_dir(&self, config_dir: &Path) -> PathBuf {
        let raw = &self.storage.data_dir;
        if raw.starts_with('~') {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home).join(raw.trim_start_matches("~/"));
            }
        }
        if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            config_dir.join(raw)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config already exists at {0}")]
    Exists(String),
    #[error("failed to read/write {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {message}")]
    Parse { path: String, message: String },
}
