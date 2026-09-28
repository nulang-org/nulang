//! Coding-agent harness primitives.
//!
//! The harness owns context accounting and the contract with an isolated
//! workspace executor. It deliberately does not own editor UI state or Nulang
//! actor-runtime internals.

use async_trait::async_trait;
use nulang_ai_core::ChangeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextObservation {
    pub resource_uri: String,
    pub revision: String,
    pub content_hash: String,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ContextLedger {
    observations: Vec<ContextObservation>,
    estimated_tokens: u64,
}

impl ContextLedger {
    pub fn observe(
        &mut self,
        resource_uri: impl Into<String>,
        revision: impl Into<String>,
        content_hash: impl Into<String>,
        estimated_tokens: u64,
    ) -> bool {
        let observation = ContextObservation {
            resource_uri: resource_uri.into(),
            revision: revision.into(),
            content_hash: content_hash.into(),
            estimated_tokens,
        };

        if self.observations.iter().any(|existing| {
            existing.resource_uri == observation.resource_uri
                && existing.revision == observation.revision
                && existing.content_hash == observation.content_hash
        }) {
            return false;
        }

        self.estimated_tokens = self.estimated_tokens.saturating_add(estimated_tokens);
        self.observations.push(observation);
        true
    }

    pub fn has_seen(&self, resource_uri: &str, revision: &str, content_hash: &str) -> bool {
        self.observations.iter().any(|observation| {
            observation.resource_uri == resource_uri
                && observation.revision == revision
                && observation.content_hash == content_hash
        })
    }

    pub fn len(&self) -> usize {
        self.observations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    pub fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    pub fn observations(&self) -> &[ContextObservation] {
        &self.observations
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFile {
    pub file_uri: String,
    pub revision: String,
    pub content_hash: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub workspace_id: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file_uri: String,
    pub severity: String,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    NotFound(String),
    RevisionMismatch { expected: String, actual: String },
    ObservationMismatch { file_uri: String },
    Execution(String),
    Other(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(resource) => write!(f, "workspace resource not found: {resource}"),
            Self::RevisionMismatch { expected, actual } => {
                write!(
                    f,
                    "workspace revision mismatch: expected {expected}, actual {actual}"
                )
            }
            Self::ObservationMismatch { file_uri } => {
                write!(f, "stale file observation: {file_uri}")
            }
            Self::Execution(message) => write!(f, "workspace execution failed: {message}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WorkspaceError {}

#[async_trait]
pub trait WorkspaceExecutor: Send + Sync {
    async fn current_revision(&self, workspace_id: &str) -> Result<String, WorkspaceError>;

    async fn read_file(
        &self,
        workspace_id: &str,
        file_uri: &str,
    ) -> Result<ObservedFile, WorkspaceError>;

    /// Apply a complete change set and return the resulting workspace revision.
    ///
    /// Implementations MUST atomically revalidate the base revision and every
    /// edit's observed content identity before committing. The harness
    /// preflight is an early-error optimization, not the authority boundary.
    async fn apply_change_set(&self, change: &ChangeSet) -> Result<String, WorkspaceError>;

    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, WorkspaceError>;

    async fn diagnostics(
        &self,
        workspace_id: &str,
        file_uri: Option<&str>,
    ) -> Result<Vec<Diagnostic>, WorkspaceError>;
}

pub struct AgentHarness<W> {
    workspace: W,
    context: ContextLedger,
}

impl<W: WorkspaceExecutor> AgentHarness<W> {
    pub fn new(workspace: W) -> Self {
        Self {
            workspace,
            context: ContextLedger::default(),
        }
    }

    pub fn context(&self) -> &ContextLedger {
        &self.context
    }

    pub fn workspace(&self) -> &W {
        &self.workspace
    }

    pub async fn read_file(
        &mut self,
        workspace_id: &str,
        file_uri: &str,
    ) -> Result<ObservedFile, WorkspaceError> {
        let observed = self.workspace.read_file(workspace_id, file_uri).await?;
        let estimated_tokens = estimate_tokens(&observed.content);
        self.context.observe(
            observed.file_uri.clone(),
            observed.revision.clone(),
            observed.content_hash.clone(),
            estimated_tokens,
        );
        Ok(observed)
    }

    pub async fn apply_change_set(&self, change: &ChangeSet) -> Result<String, WorkspaceError> {
        let current_revision = self.workspace.current_revision(&change.workspace_id).await?;
        if current_revision != change.base_revision {
            return Err(WorkspaceError::RevisionMismatch {
                expected: change.base_revision.clone(),
                actual: current_revision,
            });
        }

        for edit in &change.edits {
            let observed = self
                .workspace
                .read_file(&change.workspace_id, &edit.file_uri)
                .await?;
            if !edit.matches_observation(&observed.revision, &observed.content_hash) {
                return Err(WorkspaceError::ObservationMismatch {
                    file_uri: edit.file_uri.clone(),
                });
            }
        }

        self.workspace.apply_change_set(change).await
    }

    pub async fn exec(&self, request: ExecRequest) -> Result<ExecResult, WorkspaceError> {
        self.workspace.exec(request).await
    }

    pub async fn diagnostics(
        &self,
        workspace_id: &str,
        file_uri: Option<&str>,
    ) -> Result<Vec<Diagnostic>, WorkspaceError> {
        self.workspace.diagnostics(workspace_id, file_uri).await
    }
}

fn estimate_tokens(content: &str) -> u64 {
    let chars = content.chars().count() as u64;
    chars.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up_small_content() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
