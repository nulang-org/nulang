use async_trait::async_trait;
use nulang_ai::{
    AgentHarness, ContextLedger, Diagnostic, ExecRequest, ExecResult, ObservedFile, WorkspaceError,
    WorkspaceExecutor,
};
use nulang_ai_core::{AnchoredEdit, ChangeSet, Goal};
use std::collections::HashMap;
use std::sync::Mutex;

struct FakeWorkspace {
    revision: Mutex<String>,
    files: Mutex<HashMap<String, ObservedFile>>,
    applied: Mutex<usize>,
}

impl FakeWorkspace {
    fn new() -> Self {
        let mut files = HashMap::new();
        files.insert(
            "file://workspace/src/auth.rs".into(),
            ObservedFile {
                file_uri: "file://workspace/src/auth.rs".into(),
                revision: "rev-1".into(),
                content_hash: "blake3:auth-v1".into(),
                content: "fn authenticate() {}\n".into(),
            },
        );
        Self {
            revision: Mutex::new("rev-1".into()),
            files: Mutex::new(files),
            applied: Mutex::new(0),
        }
    }
}

#[async_trait]
impl WorkspaceExecutor for FakeWorkspace {
    async fn current_revision(&self, _workspace_id: &str) -> Result<String, WorkspaceError> {
        Ok(self.revision.lock().unwrap().clone())
    }

    async fn read_file(
        &self,
        _workspace_id: &str,
        file_uri: &str,
    ) -> Result<ObservedFile, WorkspaceError> {
        self.files
            .lock()
            .unwrap()
            .get(file_uri)
            .cloned()
            .ok_or_else(|| WorkspaceError::NotFound(file_uri.into()))
    }

    async fn apply_change_set(&self, change: &ChangeSet) -> Result<String, WorkspaceError> {
        let current = self.revision.lock().unwrap().clone();
        if current != change.base_revision {
            return Err(WorkspaceError::RevisionMismatch {
                expected: change.base_revision.clone(),
                actual: current,
            });
        }

        for edit in &change.edits {
            let observed = self.read_file(&change.workspace_id, &edit.file_uri).await?;
            if !edit.matches_observation(&observed.revision, &observed.content_hash) {
                return Err(WorkspaceError::ObservationMismatch {
                    file_uri: edit.file_uri.clone(),
                });
            }
        }

        *self.applied.lock().unwrap() += 1;
        *self.revision.lock().unwrap() = "rev-2".into();
        Ok("rev-2".into())
    }

    async fn exec(&self, _request: ExecRequest) -> Result<ExecResult, WorkspaceError> {
        Ok(ExecResult {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        })
    }

    async fn diagnostics(
        &self,
        _workspace_id: &str,
        _file_uri: Option<&str>,
    ) -> Result<Vec<Diagnostic>, WorkspaceError> {
        Ok(vec![])
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn context_ledger_deduplicates_the_same_observation_but_keeps_new_revisions() {
    let mut ledger = ContextLedger::default();

    assert!(ledger.observe(
        "file://workspace/src/auth.rs",
        "rev-1",
        "blake3:auth-v1",
        25,
    ));
    assert!(!ledger.observe(
        "file://workspace/src/auth.rs",
        "rev-1",
        "blake3:auth-v1",
        25,
    ));
    assert!(ledger.observe(
        "file://workspace/src/auth.rs",
        "rev-2",
        "blake3:auth-v2",
        30,
    ));

    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger.estimated_tokens(), 55);
}

#[test]
fn harness_reads_are_recorded_in_the_context_ledger() {
    runtime().block_on(async {
        let workspace = FakeWorkspace::new();
        let mut harness = AgentHarness::new(workspace);

        let file = harness
            .read_file("workspace-42", "file://workspace/src/auth.rs")
            .await
            .unwrap();

        assert_eq!(file.revision, "rev-1");
        assert!(harness.context().has_seen(
            "file://workspace/src/auth.rs",
            "rev-1",
            "blake3:auth-v1"
        ));
        assert!(harness.context().estimated_tokens() > 0);
    });
}

#[test]
fn harness_rejects_a_changeset_when_the_workspace_revision_is_stale() {
    runtime().block_on(async {
        let workspace = FakeWorkspace::new();
        let harness = AgentHarness::new(workspace);
        let change = ChangeSet::new(
            Goal::new("project-a", "id-seed", 0.0).id,
            "workspace-42",
            "rev-old",
            vec![],
        );

        let err = harness.apply_change_set(&change).await.unwrap_err();

        assert_eq!(
            err,
            WorkspaceError::RevisionMismatch {
                expected: "rev-old".into(),
                actual: "rev-1".into(),
            }
        );
    });
}

#[test]
fn harness_rejects_an_edit_that_does_not_match_the_content_the_agent_observed() {
    runtime().block_on(async {
        let workspace = FakeWorkspace::new();
        let harness = AgentHarness::new(workspace);
        let edit = AnchoredEdit {
            file_uri: "file://workspace/src/auth.rs".into(),
            observed_revision: "rev-1".into(),
            observed_hash: "blake3:stale".into(),
            before_anchor: None,
            after_anchor: None,
            replacement: "fn authenticate() { secure(); }\n".into(),
        };
        let change = ChangeSet::new(
            Goal::new("project-a", "id-seed", 0.0).id,
            "workspace-42",
            "rev-1",
            vec![edit],
        );

        let err = harness.apply_change_set(&change).await.unwrap_err();

        assert_eq!(
            err,
            WorkspaceError::ObservationMismatch {
                file_uri: "file://workspace/src/auth.rs".into(),
            }
        );
    });
}

#[test]
fn harness_applies_a_matching_anchored_change_and_returns_the_new_revision() {
    runtime().block_on(async {
        let workspace = FakeWorkspace::new();
        let harness = AgentHarness::new(workspace);
        let edit = AnchoredEdit {
            file_uri: "file://workspace/src/auth.rs".into(),
            observed_revision: "rev-1".into(),
            observed_hash: "blake3:auth-v1".into(),
            before_anchor: Some("fn authenticate(".into()),
            after_anchor: None,
            replacement: "fn authenticate() { secure(); }\n".into(),
        };
        let change = ChangeSet::new(
            Goal::new("project-a", "id-seed", 0.0).id,
            "workspace-42",
            "rev-1",
            vec![edit],
        );

        let revision = harness.apply_change_set(&change).await.unwrap();

        assert_eq!(revision, "rev-2");
    });
}
