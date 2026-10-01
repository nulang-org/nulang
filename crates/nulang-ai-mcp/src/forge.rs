//! Provider-neutral forge gateway with exact, time-bounded authority grants.
//!
//! Agents submit forge commands through `ForgeGateway`. The gateway checks an
//! immutable session grant before any provider backend sees the request.
//! Repository scope is intentionally exact in v1: no wildcards and no ambient
//! account-wide authority.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeOperation {
    RepoRead,
    BranchCreate,
    CommitWrite,
    ChangeCreate,
    ChangeReview,
    ChangeMerge,
    CheckRead,
}

impl fmt::Display for ForgeOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            ForgeOperation::RepoRead => "repo.read",
            ForgeOperation::BranchCreate => "branch.create",
            ForgeOperation::CommitWrite => "commit.write",
            ForgeOperation::ChangeCreate => "change.create",
            ForgeOperation::ChangeReview => "change.review",
            ForgeOperation::ChangeMerge => "change.merge",
            ForgeOperation::CheckRead => "check.read",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepositoryRef {
    pub owner: String,
    pub name: String,
}

impl RepositoryRef {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Result<Self, ForgeError> {
        let owner = owner.into();
        let name = name.into();
        validate_repository_component("owner", &owner)?;
        validate_repository_component("name", &name)?;
        Ok(Self { owner, name })
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for RepositoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

fn validate_repository_component(kind: &str, value: &str) -> Result<(), ForgeError> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));

    if valid {
        Ok(())
    } else {
        Err(ForgeError::InvalidInput(format!(
            "invalid repository {kind}: {value:?}"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeGrant {
    pub repository: RepositoryRef,
    pub operations: BTreeSet<ForgeOperation>,
    pub expires_at_unix_secs: Option<u64>,
}

impl ForgeGrant {
    pub fn new(
        repository: RepositoryRef,
        operations: impl IntoIterator<Item = ForgeOperation>,
    ) -> Self {
        Self {
            repository,
            operations: operations.into_iter().collect(),
            expires_at_unix_secs: None,
        }
    }

    pub fn with_expiry(mut self, expires_at_unix_secs: u64) -> Self {
        self.expires_at_unix_secs = Some(expires_at_unix_secs);
        self
    }

    fn permits(
        &self,
        repository: &RepositoryRef,
        operation: ForgeOperation,
        now_unix_secs: u64,
    ) -> GrantDecision {
        if &self.repository != repository || !self.operations.contains(&operation) {
            return GrantDecision::NoMatch;
        }

        if self
            .expires_at_unix_secs
            .is_some_and(|expires_at| now_unix_secs >= expires_at)
        {
            GrantDecision::Expired
        } else {
            GrantDecision::Allowed
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeSession {
    pub subject: String,
    pub grants: Vec<ForgeGrant>,
}

impl ForgeSession {
    pub fn new(subject: impl Into<String>, grants: Vec<ForgeGrant>) -> Result<Self, ForgeError> {
        let subject = subject.into();
        if subject.trim().is_empty() {
            return Err(ForgeError::InvalidInput(
                "forge subject must be non-empty".to_string(),
            ));
        }
        Ok(Self { subject, grants })
    }

    fn authorize_at(
        &self,
        repository: &RepositoryRef,
        operation: ForgeOperation,
        now_unix_secs: u64,
    ) -> Result<(), ForgeError> {
        let mut saw_expired = false;
        for grant in &self.grants {
            match grant.permits(repository, operation, now_unix_secs) {
                GrantDecision::Allowed => return Ok(()),
                GrantDecision::Expired => saw_expired = true,
                GrantDecision::NoMatch => {}
            }
        }

        if saw_expired {
            Err(ForgeError::Expired {
                subject: self.subject.clone(),
                repository: repository.full_name(),
                operation,
            })
        } else {
            Err(ForgeError::Denied {
                subject: self.subject.clone(),
                repository: repository.full_name(),
                operation,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrantDecision {
    Allowed,
    Expired,
    NoMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Rebase,
    RebaseMerge,
    Squash,
    FastForwardOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeCommand {
    ReadFile {
        repository: RepositoryRef,
        path: String,
        reference: Option<String>,
    },
    CreateBranch {
        repository: RepositoryRef,
        name: String,
        from: String,
    },
    WriteFile {
        repository: RepositoryRef,
        path: String,
        branch: String,
        content: Vec<u8>,
        message: String,
        expected_blob_sha: Option<String>,
    },
    CreateChange {
        repository: RepositoryRef,
        head: String,
        base: String,
        title: String,
        body: String,
    },
    ReviewChange {
        repository: RepositoryRef,
        number: u64,
        event: ReviewEvent,
        body: String,
        commit_id: Option<String>,
    },
    MergeChange {
        repository: RepositoryRef,
        number: u64,
        method: MergeMethod,
        expected_head_sha: Option<String>,
    },
    ListChecks {
        repository: RepositoryRef,
        reference: String,
    },
}

impl ForgeCommand {
    pub fn repository(&self) -> &RepositoryRef {
        match self {
            ForgeCommand::ReadFile { repository, .. }
            | ForgeCommand::CreateBranch { repository, .. }
            | ForgeCommand::WriteFile { repository, .. }
            | ForgeCommand::CreateChange { repository, .. }
            | ForgeCommand::ReviewChange { repository, .. }
            | ForgeCommand::MergeChange { repository, .. }
            | ForgeCommand::ListChecks { repository, .. } => repository,
        }
    }

    pub fn operation(&self) -> ForgeOperation {
        match self {
            ForgeCommand::ReadFile { .. } => ForgeOperation::RepoRead,
            ForgeCommand::CreateBranch { .. } => ForgeOperation::BranchCreate,
            ForgeCommand::WriteFile { .. } => ForgeOperation::CommitWrite,
            ForgeCommand::CreateChange { .. } => ForgeOperation::ChangeCreate,
            ForgeCommand::ReviewChange { .. } => ForgeOperation::ChangeReview,
            ForgeCommand::MergeChange { .. } => ForgeOperation::ChangeMerge,
            ForgeCommand::ListChecks { .. } => ForgeOperation::CheckRead,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: Vec<u8>,
    pub sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRef {
    pub name: String,
    pub commit_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRef {
    pub number: u64,
    pub url: Option<String>,
    pub head: String,
    pub base: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRef {
    pub id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResult {
    pub merged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: String,
    pub context: String,
    pub state: String,
    pub target_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ForgeResponse {
    File(FileContent),
    Branch(BranchRef),
    Commit(CommitRef),
    Change(ChangeRef),
    Review(ReviewRef),
    Merge(MergeResult),
    Checks(Vec<CheckRun>),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ForgeError {
    #[error("forge authority denied for {subject} on {repository}: {operation}")]
    Denied {
        subject: String,
        repository: String,
        operation: ForgeOperation,
    },
    #[error("forge authority expired for {subject} on {repository}: {operation}")]
    Expired {
        subject: String,
        repository: String,
        operation: ForgeOperation,
    },
    #[error("forge approval required on {repository}: {operation}: {reason}")]
    ApprovalRequired {
        repository: String,
        operation: ForgeOperation,
        reason: String,
    },
    #[error("forge policy denied on {repository}: {operation}: {reason}")]
    PolicyDenied {
        repository: String,
        operation: ForgeOperation,
        reason: String,
    },
    #[error("forge outcome uncertain on {repository}: {operation}: {reason}")]
    Uncertain {
        repository: String,
        operation: ForgeOperation,
        reason: String,
    },
    #[error("invalid forge input: {0}")]
    InvalidInput(String),
    #[error("forge backend error: {0}")]
    Backend(String),
    #[error("forge protocol error: {0}")]
    Protocol(String),
}

#[async_trait]
pub trait ForgeBackend: Send + Sync {
    async fn execute(&self, command: ForgeCommand) -> Result<ForgeResponse, ForgeError>;
}

pub struct ForgeGateway<B> {
    backend: B,
}

impl<B> ForgeGateway<B>
where
    B: ForgeBackend,
{
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub async fn execute(
        &self,
        session: &ForgeSession,
        command: ForgeCommand,
    ) -> Result<ForgeResponse, ForgeError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ForgeError::Backend("system clock is before Unix epoch".to_string()))?
            .as_secs();
        self.execute_at(session, command, now).await
    }

    pub async fn execute_at(
        &self,
        session: &ForgeSession,
        command: ForgeCommand,
        now_unix_secs: u64,
    ) -> Result<ForgeResponse, ForgeError> {
        session.authorize_at(command.repository(), command.operation(), now_unix_secs)?;
        self.backend.execute(command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn block_on_immediate<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future unexpectedly yielded"),
        }
    }

    struct RecordingBackend {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ForgeBackend for RecordingBackend {
        async fn execute(&self, _command: ForgeCommand) -> Result<ForgeResponse, ForgeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ForgeResponse::Checks(Vec::new()))
        }
    }

    fn repo(name: &str) -> RepositoryRef {
        RepositoryRef::new("nulang-org", name).unwrap()
    }

    fn read_command(repository: RepositoryRef) -> ForgeCommand {
        ForgeCommand::ReadFile {
            repository,
            path: "README.md".to_string(),
            reference: Some("main".to_string()),
        }
    }

    #[test]
    fn exact_repository_read_grant_allows_backend_call() {
        let backend = RecordingBackend {
            calls: AtomicUsize::new(0),
        };
        let gateway = ForgeGateway::new(backend);
        let session = ForgeSession::new(
            "agent-7",
            vec![ForgeGrant::new(repo("nulang"), [ForgeOperation::RepoRead])],
        )
        .unwrap();

        let result =
            block_on_immediate(gateway.execute_at(&session, read_command(repo("nulang")), 100));

        assert!(result.is_ok());
        assert_eq!(gateway.backend().calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn grant_for_other_repository_fails_before_backend_call() {
        let backend = RecordingBackend {
            calls: AtomicUsize::new(0),
        };
        let gateway = ForgeGateway::new(backend);
        let session = ForgeSession::new(
            "agent-7",
            vec![ForgeGrant::new(repo("other"), [ForgeOperation::RepoRead])],
        )
        .unwrap();

        let error =
            block_on_immediate(gateway.execute_at(&session, read_command(repo("nulang")), 100))
                .unwrap_err();

        assert!(matches!(error, ForgeError::Denied { .. }));
        assert_eq!(gateway.backend().calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn expired_grant_fails_before_backend_call() {
        let backend = RecordingBackend {
            calls: AtomicUsize::new(0),
        };
        let gateway = ForgeGateway::new(backend);
        let session = ForgeSession::new(
            "agent-7",
            vec![ForgeGrant::new(repo("nulang"), [ForgeOperation::RepoRead]).with_expiry(100)],
        )
        .unwrap();

        let error =
            block_on_immediate(gateway.execute_at(&session, read_command(repo("nulang")), 100))
                .unwrap_err();

        assert!(matches!(error, ForgeError::Expired { .. }));
        assert_eq!(gateway.backend().calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn commit_write_does_not_imply_merge_authority() {
        let backend = RecordingBackend {
            calls: AtomicUsize::new(0),
        };
        let gateway = ForgeGateway::new(backend);
        let repository = repo("nulang");
        let session = ForgeSession::new(
            "coder-agent",
            vec![ForgeGrant::new(
                repository.clone(),
                [ForgeOperation::CommitWrite],
            )],
        )
        .unwrap();

        let error = block_on_immediate(gateway.execute_at(
            &session,
            ForgeCommand::MergeChange {
                repository,
                number: 42,
                method: MergeMethod::Squash,
                expected_head_sha: None,
            },
            100,
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            ForgeError::Denied {
                operation: ForgeOperation::ChangeMerge,
                ..
            }
        ));
        assert_eq!(gateway.backend().calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn repository_components_reject_path_injection() {
        assert!(RepositoryRef::new("../admin", "repo").is_err());
        assert!(RepositoryRef::new("owner", "repo/name").is_err());
        assert!(RepositoryRef::new("owner", "valid.repo-name_1").is_ok());
    }
}
