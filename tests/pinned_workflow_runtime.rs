//! Runtime lifecycle integration for pinned durable workflows.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorArtifact, BehaviorManifest, BehaviorPackage, BEHAVIOR_ARTIFACT_KIND_NBC_V1,
    BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    ActorSnapshot, DurableTransition, ImmutablePinnedArtifactResolver, MemoryStore,
    PersistenceStore, PinnedArtifactBundle, Runtime, StateModel, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};
use nulang::vm::Value;
use std::collections::HashMap;
use std::io::{self, ErrorKind};

fn manifest(bytes: &[u8]) -> BehaviorManifest {
    let identity = ArtifactIdentityManifest::new(
        None,
        SemanticId::from_canonical_bytes(b"pinned-runtime-workflow", []),
        "compiler-v1",
        "bytecode",
        "abi-v1",
        "bytecode",
        std::iter::empty::<&str>(),
    );
    BehaviorManifest {
        schema: BEHAVIOR_MANIFEST_SCHEMA.into(),
        package: BehaviorPackage {
            name: "payment".into(),
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
        actors: Vec::new(),
    }
}

fn workflow_state() -> HashMap<String, StateModel> {
    HashMap::from([("count".into(), StateModel::Durable)])
}

fn init() -> Box<dyn FnOnce() -> Vec<(String, Value)>> {
    Box::new(|| vec![("count".into(), Value::int(7))])
}

struct FixtureResolver {
    expected_digest: String,
    artifact: Option<PinnedArtifactBundle>,
}

impl ImmutablePinnedArtifactResolver for FixtureResolver {
    fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
        if digest == self.expected_digest {
            Ok(self.artifact.clone())
        } else {
            Ok(None)
        }
    }
}

#[test]
fn runtime_pinned_start_uses_one_atomic_transition_before_publication() {
    let bytes = b"payment-workflow-v1";
    let behavior = manifest(bytes);
    let mut runtime = Runtime::new();
    let actor_id = runtime.try_spawn_pinned_workflow_actor(
        "Payment", "payment/1", init(), workflow_state(), &behavior, bytes,
    ).unwrap();

    assert!(runtime.actors.contains_key(&actor_id));
    assert_eq!(runtime.persistence.latest_sequence(actor_id), 1);
    assert_eq!(runtime.persistence.load_snapshot(actor_id).unwrap().sequence, 1);
    let events = runtime.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], WorkflowEvent::WorkflowStarted { sequence: 1, .. }));
    assert_eq!(
        runtime.actors[&actor_id].get_state_field("count").unwrap().as_int(),
        Some(7),
    );
    assert_eq!(runtime.actors[&actor_id].sequence, 1);
}

#[test]
fn legacy_recovery_cannot_activate_a_pinned_execution() {
    let bytes = b"payment-workflow-v1";
    let behavior = manifest(bytes);
    let mut runtime = Runtime::new();
    let actor_id = runtime.try_spawn_pinned_workflow_actor(
        "Payment", "payment/2", init(), workflow_state(), &behavior, bytes,
    ).unwrap();
    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), None);
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn resolver_mismatch_or_absence_does_not_publish_recovered_actor() {
    let bytes = b"payment-workflow-v1";
    let behavior = manifest(bytes);
    let mut runtime = Runtime::new();
    let actor_id = runtime.try_spawn_pinned_workflow_actor(
        "Payment", "payment/3", init(), workflow_state(), &behavior, bytes,
    ).unwrap();
    runtime.actors.remove(&actor_id);
    let missing = FixtureResolver {
        expected_digest: behavior.artifact.digest.clone(),
        artifact: None,
    };
    assert_eq!(
        runtime.recover_pinned_workflow_actor(actor_id, "payment/3", &missing)
            .unwrap_err().kind(),
        ErrorKind::NotFound,
    );
    let changed = b"payment-workflow-v2";
    let wrong_artifact = FixtureResolver {
        expected_digest: behavior.artifact.digest.clone(),
        artifact: Some(PinnedArtifactBundle {
            manifest: manifest(changed),
            executable: changed.to_vec(),
        }),
    };
    assert_eq!(
        runtime.recover_pinned_workflow_actor(actor_id, "payment/3", &wrong_artifact)
            .unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn verified_immutable_artifact_recovers_the_original_execution() {
    let bytes = b"payment-workflow-v1";
    let behavior = manifest(bytes);
    let mut runtime = Runtime::new();
    let actor_id = runtime.try_spawn_pinned_workflow_actor(
        "Payment", "payment/4", init(), workflow_state(), &behavior, bytes,
    ).unwrap();
    runtime.actors.remove(&actor_id);
    let resolver = FixtureResolver {
        expected_digest: behavior.artifact.digest.clone(),
        artifact: Some(PinnedArtifactBundle {
            manifest: behavior,
            executable: bytes.to_vec(),
        }),
    };
    assert_eq!(
        runtime.recover_pinned_workflow_actor(actor_id, "payment/4", &resolver).unwrap(),
        actor_id,
    );
    assert!(runtime.actors.contains_key(&actor_id));
}

#[test]
fn legacy_backend_remains_available_but_cannot_be_used_for_pinned_creation() {
    use nulang::runtime::JsonFileStore;
    let directory = std::env::temp_dir().join(format!("nulang-pinned-runtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(JsonFileStore::new(&directory).unwrap());
    let bytes = b"payment-workflow-v1";
    assert_eq!(
        runtime.try_spawn_pinned_workflow_actor(
            "Payment", "payment/5", init(), workflow_state(), &manifest(bytes), bytes,
        ).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
    assert!(runtime.actors.is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn runtime_reopen_requires_exact_pinned_artifact_before_actor_publication() {
    use nulang::runtime::LibsqlStore;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!("nulang-runtime-pinned-{nonce}"));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("workflow.db");
    let bytes = b"workflow-v1";
    let behavior = manifest(bytes);
    let actor_id = {
        let mut first = Runtime::new();
        first.persistence = Box::new(LibsqlStore::new(&path).unwrap());
        first.try_spawn_pinned_workflow_actor(
            "Payment", "payment/restart", init(), workflow_state(), &behavior, bytes,
        ).unwrap()
    };

    let mut next = Runtime::new();
    next.persistence = Box::new(LibsqlStore::new(&path).unwrap());
    assert_eq!(next.recover_actor(actor_id), None);
    let resolver = FixtureResolver {
        expected_digest: behavior.artifact.digest.clone(),
        artifact: Some(PinnedArtifactBundle {
            manifest: behavior,
            executable: bytes.to_vec(),
        }),
    };
    assert_eq!(
        next.recover_pinned_workflow_actor(actor_id, "payment/restart", &resolver).unwrap(),
        actor_id,
    );
    drop(next);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unverified_atomic_start_without_decodable_pin_is_not_treated_as_legacy() {
    let mut store = MemoryStore::new();
    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 123,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id: 123,
            sequence: 1,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::WorkflowStarted {
            sequence: 1,
            name: "Payment".into(),
            state: Vec::new(),
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }).unwrap();

    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(store);
    assert_eq!(runtime.recover_actor(123), None);
    assert!(!runtime.actors.contains_key(&123));
    let bytes = b"payment-workflow-v1";
    let resolver = FixtureResolver {
        expected_digest: manifest(bytes).artifact.digest.clone(),
        artifact: Some(PinnedArtifactBundle {
            manifest: manifest(bytes),
            executable: bytes.to_vec(),
        }),
    };
    assert_eq!(
        runtime.recover_pinned_workflow_actor(123, "payment/123", &resolver)
            .unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&123));
}

#[cfg(feature = "sqlite")]
#[test]
fn pinned_runtime_process_recovery_helper() {
    use nulang::runtime::LibsqlStore;
    let Ok(mode) = std::env::var("NULANG_PIN_RUNTIME_TEST_MODE") else {
        return;
    };
    let db = std::path::PathBuf::from(
        std::env::var_os("NULANG_PIN_RUNTIME_TEST_DB").expect("test db path"),
    );
    let id_path = db.with_extension("actor-id");
    let bytes = b"runtime-process-immutable-v1";
    let behavior = manifest(bytes);
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(LibsqlStore::new(&db).unwrap());

    match mode.as_str() {
        "create" => {
            let actor_id = runtime.try_spawn_pinned_workflow_actor(
                "Payment", "payment/process-restart", init(), workflow_state(), &behavior, bytes,
            ).unwrap();
            std::fs::write(id_path, actor_id.to_string()).unwrap();
        }
        "resume" | "changed" => {
            let actor_id = std::fs::read_to_string(id_path)
                .unwrap().parse::<u64>().unwrap();
            assert_eq!(runtime.recover_actor(actor_id), None);
            let selected: &[u8] = if mode == "changed" {
                b"runtime-process-immutable-v2"
            } else {
                bytes
            };
            let resolver = FixtureResolver {
                expected_digest: behavior.artifact.digest.clone(),
                artifact: Some(PinnedArtifactBundle {
                    manifest: manifest(selected),
                    executable: selected.to_vec(),
                }),
            };
            let result = runtime.recover_pinned_workflow_actor(
                actor_id, "payment/process-restart", &resolver,
            );
            if mode == "changed" {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
                assert!(!runtime.actors.contains_key(&actor_id));
            } else {
                assert_eq!(result.unwrap(), actor_id);
                assert!(runtime.actors.contains_key(&actor_id));
            }
        }
        unexpected => panic!("invalid child mode: {unexpected}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn pinned_runtime_recovers_across_real_process_boundaries() {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-runtime-pinned-process-{}-{nonce}", std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let db = directory.join("workflow.db");

    for mode in ["create", "changed", "resume"] {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pinned_runtime_process_recovery_helper"])
            .env("NULANG_PIN_RUNTIME_TEST_MODE", mode)
            .env("NULANG_PIN_RUNTIME_TEST_DB", &db)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{mode}: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("1 passed"),
            "{mode} child did not execute: {}",
            String::from_utf8_lossy(&child.stdout),
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
