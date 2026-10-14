//! Atomic creation and strict recovery preflight for pinned workflow history.
//! Tests use real persistence stores; no provider/external effect is simulated.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorActor, BehaviorArtifact, BehaviorManifest, BehaviorPackage, BehaviorPersistence,
    MigrationIdentityCoverage, BEHAVIOR_ARTIFACT_KIND_NBC_V1, BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    commit_pinned_workflow_start, verify_pinned_workflow_resume, ActorSnapshot,
    DurableTransition, MemoryStore, PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use std::io::ErrorKind;

fn manifest(bytes: &[u8]) -> BehaviorManifest {
    let semantic_id = SemanticId::from_canonical_bytes(b"pinned-workflow-test", []);
    let identity = ArtifactIdentityManifest::new(
        None, semantic_id, "compiler-v1", "bytecode", "abi-v1", "bytecode",
        std::iter::empty::<&str>(),
    );
    BehaviorManifest {
        schema: BEHAVIOR_MANIFEST_SCHEMA.into(),
        package: BehaviorPackage {
            name: "payments".into(),
            version: "1.0.0".into(),
            language_version: "1".into(),
        },
        artifact: BehaviorArtifact {
            kind: BEHAVIOR_ARTIFACT_KIND_NBC_V1.into(),
            digest: format!("blake3:{}", blake3::hash(bytes).to_hex()),
            source_id: None,
            semantic_id: identity.semantic_id().to_string(),
            artifact_id: identity.artifact_id().to_string(),
            compiler_version: identity.compiler_version().into(),
            target: identity.target().into(),
            abi: identity.abi().into(),
            backend: identity.backend().into(),
            flags: Vec::new(),
        },
        actors: vec![BehaviorActor {
            name: "payments::Order".into(),
            persistence: BehaviorPersistence::Durable,
            schema_version: 1,
            state_schema_semantic_id: SemanticId::from_canonical_bytes(b"order-v1", [])
                .to_string(),
            migration_identity: MigrationIdentityCoverage::TopologyOnly,
            migrations: Vec::new(),
        }],
    }
}

fn snapshot(actor_id: u64) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        sequence: 1,
        activation_epoch: 1,
        ..ActorSnapshot::default()
    }
}

#[test]
fn creation_atomically_commits_snapshot_start_and_pin() {
    let bytes = b"workflow-binary-v1";
    let behavior = manifest(bytes);
    let mut store = MemoryStore::new();
    let receipt = commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &behavior, bytes, snapshot(42), vec![],
    ).unwrap();
    assert_eq!(receipt.sequence, 1);
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 1);
    let events = store.read_workflow_events(42);
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], WorkflowEvent::WorkflowStarted { sequence: 1, name, .. } if name == "Order"));
    verify_pinned_workflow_resume(&store, 42, "payments/42", &behavior, bytes).unwrap();
}

#[test]
fn incompatible_binary_rejected_before_resume() {
    let bytes = b"workflow-binary-v1";
    let mut store = MemoryStore::new();
    commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &manifest(bytes), bytes, snapshot(42), vec![],
    ).unwrap();

    let changed = b"workflow-binary-v2";
    let error = verify_pinned_workflow_resume(
        &store, 42, "payments/42", &manifest(changed), changed,
    ).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    verify_pinned_workflow_resume(
        &store, 42, "payments/42", &manifest(bytes), bytes,
    ).unwrap();
}

#[test]
fn missing_pin_fails_closed_even_when_a_snapshot_exists() {
    let bytes = b"artifact";
    let mut store = MemoryStore::new();
    store.save_snapshot(snapshot(42)).unwrap();
    let error = verify_pinned_workflow_resume(
        &store, 42, "payments/42", &manifest(bytes), bytes,
    ).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NotFound | ErrorKind::InvalidData));
}

#[test]
fn conflicting_creation_cannot_replace_existing_execution_pin() {
    let bytes = b"workflow-v1";
    let mut store = MemoryStore::new();
    commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &manifest(bytes), bytes, snapshot(42), vec![],
    ).unwrap();
    let changed = b"workflow-v2";
    let result = commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &manifest(changed), changed, snapshot(42), vec![],
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::AlreadyExists);
    verify_pinned_workflow_resume(&store, 42, "payments/42", &manifest(bytes), bytes).unwrap();
}

#[test]
fn invalid_snapshot_is_rejected_without_any_committed_state() {
    let bytes = b"workflow-v1";
    let mut store = MemoryStore::new();
    let error = commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &manifest(bytes), bytes, snapshot(43), vec![],
    ).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(store.load_snapshot(42).is_none());
    assert!(store.read_workflow_events(42).is_empty());
}

#[test]
fn pin_requires_the_atomic_commit_backend() {
    use nulang::runtime::JsonFileStore;
    let base = std::env::temp_dir().join(format!("nulang-pin-unsupported-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let mut store = JsonFileStore::new(&base).unwrap();
    let bytes = b"binary";
    let error = commit_pinned_workflow_start(
        &mut store, 42, "payments/42", "Order", &manifest(bytes), bytes, snapshot(42), vec![],
    ).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn pin_survives_reopening_libsql_and_rejects_replaced_executable() {
    use nulang::runtime::LibsqlStore;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!("nulang-pinned-restart-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("pinned.db");
    let bytes = b"workflow-v1";
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        commit_pinned_workflow_start(
            &mut store, 77, "payments/77", "Order", &manifest(bytes), bytes, snapshot(77), vec![],
        ).unwrap();
    }
    {
        let store = LibsqlStore::new(&path).unwrap();
        verify_pinned_workflow_resume(&store, 77, "payments/77", &manifest(bytes), bytes).unwrap();
        let replacement = b"workflow-v2";
        let error = verify_pinned_workflow_resume(
            &store, 77, "payments/77", &manifest(replacement), replacement,
        ).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn pinned_workflow_process_restart_helper() {
    use nulang::runtime::LibsqlStore;

    let Ok(mode) = std::env::var("NULANG_PIN_TEST_CHILD_MODE") else {
        return;
    };
    let path = std::path::PathBuf::from(
        std::env::var_os("NULANG_PIN_TEST_CHILD_DATABASE").expect("test database path"),
    );
    let bytes = b"workflow-v1";
    match mode.as_str() {
        "create" => {
            let mut store = LibsqlStore::new(&path).unwrap();
            commit_pinned_workflow_start(
                &mut store, 88, "payments/88", "Order", &manifest(bytes), bytes,
                snapshot(88), vec![],
            ).unwrap();
        }
        "resume" => {
            let store = LibsqlStore::new(&path).unwrap();
            verify_pinned_workflow_resume(
                &store, 88, "payments/88", &manifest(bytes), bytes,
            ).unwrap();
        }
        "changed" => {
            let store = LibsqlStore::new(&path).unwrap();
            let different = b"workflow-v2";
            assert_eq!(
                verify_pinned_workflow_resume(
                    &store, 88, "payments/88", &manifest(different), different,
                ).unwrap_err().kind(),
                ErrorKind::InvalidData
            );
        }
        unexpected => panic!("unexpected test child mode: {unexpected}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn pinned_workflow_real_process_restart_verifies_exact_artifact() {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-pin-process-restart-{}-{nonce}", std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("workflow.db");

    for mode in ["create", "resume", "changed"] {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pinned_workflow_process_restart_helper"])
            .env("NULANG_PIN_TEST_CHILD_MODE", mode)
            .env("NULANG_PIN_TEST_CHILD_DATABASE", &path)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child {mode} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "child {mode} was not actually exercised: {}",
            String::from_utf8_lossy(&child.stdout),
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn atomic_history_without_execution_pin_cannot_be_resumed_as_pinned() {
    let mut store = MemoryStore::new();
    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 54,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: Some(snapshot(54)),
        workflow_events: vec![WorkflowEvent::WorkflowStarted {
            sequence: 1,
            name: "Order".into(),
            state: vec![],
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();
    let bytes = b"workflow-v1";
    assert_eq!(
        verify_pinned_workflow_resume(&store, 54, "payments/54", &manifest(bytes), bytes)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
}

#[test]
fn appended_fake_execution_pin_is_rejected_on_recovery() {
    let mut store = MemoryStore::new();
    let bytes = b"workflow-v1";
    commit_pinned_workflow_start(
        &mut store, 55, "payments/55", "Order", &manifest(bytes), bytes,
        snapshot(55), vec![],
    ).unwrap();

    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 55,
        activation_epoch: 1,
        sequence: 2,
        expected_previous_sequence: 1,
        command: None,
        snapshot: None,
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: 2,
            replay_id: None,
            name: "__nulang.execution_pin/v1".into(),
            args: vec![],
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();

    assert_eq!(
        verify_pinned_workflow_resume(&store, 55, "payments/55", &manifest(bytes), bytes)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
}

#[test]
fn later_workflow_started_event_cannot_be_accepted_as_one_execution() {
    let mut store = MemoryStore::new();
    let bytes = b"workflow-v1";
    commit_pinned_workflow_start(
        &mut store, 56, "payments/56", "Order", &manifest(bytes), bytes,
        snapshot(56), vec![],
    ).unwrap();

    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 56,
        activation_epoch: 1,
        sequence: 2,
        expected_previous_sequence: 1,
        command: None,
        snapshot: None,
        workflow_events: vec![WorkflowEvent::WorkflowStarted {
            sequence: 2,
            name: "SecondStart".into(),
            state: vec![],
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();

    assert_eq!(
        verify_pinned_workflow_resume(&store, 56, "payments/56", &manifest(bytes), bytes)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
}

#[test]
fn identical_initialization_is_not_a_second_successful_creation() {
    let bytes = b"stable-workflow";
    let behavior = manifest(bytes);
    let mut store = MemoryStore::new();
    commit_pinned_workflow_start(
        &mut store, 400, "payments/400", "Order", &behavior, bytes,
        snapshot(400), vec![],
    ).unwrap();
    assert_eq!(
        commit_pinned_workflow_start(
            &mut store, 400, "payments/400", "Order", &behavior, bytes,
            snapshot(400), vec![],
        ).unwrap_err().kind(),
        ErrorKind::AlreadyExists,
    );
    assert_eq!(store.read_workflow_events(400).len(), 2);
}
