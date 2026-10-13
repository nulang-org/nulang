//! Read-only compiler tools exposed to a local, trusted MCP stdio host.
//! A caller chooses only a source path, never compiler arguments or a command.

use crate::{ToolHandler, ToolRegistry, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use std::sync::Arc;

#[derive(Clone)]
pub struct LocalToolConfig {
    /// Canonicalized, host-selected workspace root. Must not be supplied by MCP callers.
    pub workspace: PathBuf,
    /// Host-selected compiler executable. Never accept this in tool arguments.
    pub compiler: PathBuf,
    pub timeout: Duration,
}

impl LocalToolConfig {
    pub fn new(workspace: PathBuf, compiler: PathBuf) -> Self {
        Self {
            workspace,
            compiler,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

struct CheckTool {
    config: LocalToolConfig,
}

#[async_trait]
impl ToolHandler for CheckTool {
    async fn call(&self, args: Value) -> Result<Value, String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "path must be a relative .nula source file".to_string())?;
        let source = resolve_source(&self.config.workspace, path)?;
        // --check performs static validation, not VM execution. Arguments and
        // current directory are fixed by the trusted server, not the model.
        let mut command = Command::new(&self.config.compiler);
        command
            .current_dir(&self.config.workspace)
            .arg("--json")
            .arg("--check")
            .arg(&source)
            .kill_on_drop(true);
        let output = tokio::time::timeout(self.config.timeout, command.output())
            .await
            .map_err(|_| "Nulang compiler timed out".to_string())?
            .map_err(|err| format!("Unable to invoke Nulang compiler: {err}"))?;
        if output.stdout.len() > 4 * 1024 * 1024 {
            return Err("Compiler diagnostics exceeded 4 MiB output limit".into());
        }
        let report: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "Compiler failed to return valid JSON diagnostics".to_string())?;
        if report.get("schema_version").and_then(Value::as_u64).is_none()
            || report.get("diagnostics").and_then(Value::as_array).is_none()
            || report.get("ok").and_then(Value::as_bool).is_none()
        {
            return Err("Compiler returned an unexpected diagnostic schema".into());
        }
        Ok(json!({
            "exit_success": output.status.success(),
            "report": report
        }))
    }

    fn errors_as_tool_results(&self) -> bool {
        true
    }
}

/// Resolve only existing workspace files; canonicalization rejects symlinks
/// that point outside the host's root even when the lexical path appears safe.
fn resolve_source(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() || relative.len() > 4096 {
        return Err("Source path is empty or too long".into());
    }
    let path = Path::new(relative);
    if !path.components().all(|part| matches!(part, Component::Normal(_))) {
        return Err("Source path must be relative and cannot contain traversal".into());
    }
    if path.extension().and_then(|s| s.to_str()) != Some("nula") {
        return Err("Only .nula source files may be checked".into());
    }
    let real = root
        .join(path)
        .canonicalize()
        .map_err(|_| "Source file was not found".to_string())?;
    if !real.starts_with(root) || !real.is_file() {
        return Err("Source file must stay inside the workspace".into());
    }
    let size = real
        .metadata()
        .map_err(|_| "Cannot read source metadata".to_string())?
        .len();
    if size > 2 * 1024 * 1024 {
        return Err("Source file exceeds the 2 MiB check limit".into());
    }
    Ok(real)
}

pub async fn register_local_tools(registry: &Arc<ToolRegistry>, config: LocalToolConfig) {
    registry
        .register(
            ToolSpec {
                name: "nulang_check".into(),
                description: "Statically check an existing Nulang source file in the trusted workspace; return versioned JSON diagnostics. Does not execute the program or edit files.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Workspace-relative path to an existing .nula file"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            Arc::new(CheckTool { config }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_absolute_and_parent_paths() {
        let root = std::env::temp_dir();
        assert!(resolve_source(&root, "/etc/passwd").is_err());
        assert!(resolve_source(&root, "../x.nula").is_err());
        assert!(resolve_source(&root, "./x.nula").is_err());
        assert!(resolve_source(&root, "x/../y.nula").is_err());
        assert!(resolve_source(&root, "x.txt").is_err());
    }
}
