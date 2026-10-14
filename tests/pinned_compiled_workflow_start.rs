//! Compiler-backed pinned creation must install verified NBC before the
//! initial atomic snapshot is committed or the actor enters the scheduler.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorActor, BehaviorArtifact, BehaviorManifest, BehaviorPackage, BehaviorPersistence,
    MigrationIdentityCoverage, BEHAVIOR_ARTIFACT_KIND_NBC_V1, BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::bytecode::{ActorMeta, BehaviorTableEntry, CodeModule, Constant, Instruction};
use nulang::content_identity::SemanticId;
use nulang::runtime::{JsonFileStore, Runtime, WorkflowEvent};
use std::io::ErrorKind;

fn compiled_payment() -> CodeModule {
    let mut module = CodeModule::new("payments-compiled");
    module.instructions.push(Instruction::decode(0x57000000).unwrap());
    module.behaviors.push(BehaviorTableEntry {
        name: "Payment.charge".into(),
        param_count: 0,
        code_offset: 0,
        local_count: 1,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    let mut payment = ActorMeta::new("Payment");
    payment.persistent = true;
    payment.is_workflow = true;
    payment.behavior_indices = vec![0];
    payment.state_models = vec![
        ("counter".into(), nulang::ast::StateModel::Durable),
        ("reference".into(), nulang::ast::StateModel::Durable),
    ];
    payment.state_defaults = vec![
        ("counter".into(), Constant::Int(7)),
        ("reference".into(), Constant::String("initial-value".into())),
    ];
    module.add_actor_meta(payment);
    module
}

fn manifest(bytes: &[u8]) -> BehaviorManifest {
    let identity = ArtifactIdentityManifest::new(
        None,
        SemanticId::from_canonical_bytes(b"compiled-payment", []),
        "compiler-v1",
        "nulang-bytecode-v1",
        "abi-v1",
        "bytecode",
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
            compiler_version: identity.compiler_version().to_string(),
            target: identity.target().to_string(),
            abi: identity.abi().to_string(),
            backend: identity.backend().to_string(),
            flags: vec![],
        },
        actors: vec![BehaviorActor {
            name: "Payment".into(),
            persistence: BehaviorPersistence::Durable,
            schema_version: 1,
            state_schema_semantic_id: SemanticId::from_canonical_bytes(b"payment-state", [])
                .to_string(),
            migration_identity: MigrationIdentityCoverage::TopologyOnly,
            migrations: vec![],
        }],
    }
}

#[test]
fn compiled_pinned_spawn_commits_schema_and_default_state_before_publication() {
    let bytes = compiled_payment().to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .try_spawn_pinned_compiled_workflow(
            "payments/compiled/1", "Payment", &manifest(&bytes), &bytes,
        )
        .unwrap();

    let actor = &runtime.actors[&actor_id];
    assert!(actor.persistent && actor.is_workflow);
    assert_eq!(actor.name, "Payment");
    assert_eq!(actor.sequence, 1);
    assert_eq!(actor.get_state_field("counter").and_then(|v| v.as_int()), Some(7));
    assert_eq!(actor.bytecode_offsets, vec![0]);
    assert_eq!(
        actor.bytecode_module.as_ref().expect("verified NBC").name,
        "payments-compiled",
    );
    assert_eq!(actor.behavior_table.len(), 2, "step and internal timer behavior");
    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.schema_name.as_deref(), Some("Payment"));
    assert_eq!(snapshot.sequence, 1);
    assert!(matches!(
        snapshot.state.get("counter"),
        Some(nulang::runtime::PersistedValue::Int(7)),
    ));
    assert!(matches!(
        snapshot.state.get("reference"),
        Some(nulang::runtime::PersistedValue::String(value)) if value == "initial-value",
    ));
    let history = runtime.persistence.read_workflow_events(actor_id);
    assert_eq!(history.len(), 2);
    assert!(matches!(
        &history[0],
        WorkflowEvent::WorkflowStarted { sequence: 1, name, state }
            if name == "Payment"
                && state.iter().any(|value| matches!(
                    value, nulang::runtime::PersistedValue::String(s) if s == "initial-value"
                )),
    ));
}

#[test]
fn invalid_nbc_is_rejected_without_publishing_or_persisting_any_workflow() {
    let bytes = b"not-a-real-nbc";
    let mut runtime = Runtime::new();
    let result = runtime.try_spawn_pinned_compiled_workflow(
        "payments/invalid", "Payment", &manifest(bytes), bytes,
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(runtime.actors.is_empty());
}

#[test]
fn wrong_manifest_digest_fails_before_actor_publication() {
    let bytes = compiled_payment().to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    let result = runtime.try_spawn_pinned_compiled_workflow(
        "payments/altered", "Payment", &manifest(b"other bytes"), &bytes,
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(runtime.actors.is_empty());
}

#[test]
fn missing_or_ambiguous_compiled_schema_is_rejected() {
    let bytes = compiled_payment().to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    assert_eq!(
        runtime.try_spawn_pinned_compiled_workflow(
            "payments/missing", "Other", &manifest(&bytes), &bytes,
        ).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(runtime.actors.is_empty());
}

#[test]
fn invalid_behavior_reference_fails_before_atomic_start() {
    let mut module = compiled_payment();
    module.actor_metadata[0].behavior_indices = vec![999];
    let bytes = module.to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    let result = runtime.try_spawn_pinned_compiled_workflow(
        "payments/invalid-index", "Payment", &manifest(&bytes), &bytes,
    );
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(runtime.actors.is_empty());
}


#[test]
fn duplicate_compiler_state_defaults_are_rejected_before_creation() {
    let mut module = compiled_payment();
    module.actor_metadata[0]
        .state_defaults
        .push(("counter".into(), Constant::Int(99)));
    let bytes = module.to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    assert_eq!(
        runtime.try_spawn_pinned_compiled_workflow(
            "payments/duplicate-default", "Payment", &manifest(&bytes), &bytes,
        ).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(runtime.actors.is_empty());
}

#[test]
fn non_state_compiler_constant_cannot_be_used_as_durable_default() {
    let mut module = compiled_payment();
    module.actor_metadata[0]
        .state_defaults
        .push(("unsupported".into(), Constant::FunctionRef(0)));
    let bytes = module.to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    assert_eq!(
        runtime.try_spawn_pinned_compiled_workflow(
            "payments/unsupported-default", "Payment", &manifest(&bytes), &bytes,
        ).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(runtime.actors.is_empty());
}

#[test]
fn unsupported_atomic_backend_never_publishes_compiled_workflow() {
    let directory = std::env::temp_dir().join(format!(
        "nulang-compiled-pinned-json-{}", std::process::id(),
    ));
    let _ = std::fs::remove_dir_all(&directory);
    let bytes = compiled_payment().to_nbc(None).unwrap();
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(JsonFileStore::new(&directory).unwrap());
    assert_eq!(
        runtime.try_spawn_pinned_compiled_workflow(
            "payments/no-atomic", "Payment", &manifest(&bytes), &bytes,
        ).unwrap_err().kind(),
        ErrorKind::Unsupported,
    );
    assert!(runtime.actors.is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn compiled_pinned_workflow_recovers_from_immutable_artifact_after_process_restart() {
    use nulang::runtime::{ImmutablePinnedArtifactResolver, LibsqlStore, PinnedArtifactBundle};
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Resolver { bundle: PinnedArtifactBundle }
    impl ImmutablePinnedArtifactResolver for Resolver {
        fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
            Ok((digest == self.bundle.manifest.artifact.digest).then(|| self.bundle.clone()))
        }
    }

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-compiled-pinned-restart-{}-{nonce}", std::process::id(),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("compiled.db");
    let bytes = compiled_payment().to_nbc(None).unwrap();
    let behavior = manifest(&bytes);

    let actor_id = {
        let mut original = Runtime::new();
        original.persistence = Box::new(LibsqlStore::new(&path).unwrap());
        original.try_spawn_pinned_compiled_workflow(
            "payments/compiled/restart", "Payment", &behavior, &bytes,
        ).unwrap()
    };

    let mut restarted = Runtime::new();
    restarted.persistence = Box::new(LibsqlStore::new(&path).unwrap());
    assert_eq!(restarted.recover_actor(actor_id), None);
    let resolver = Resolver { bundle: PinnedArtifactBundle {
        manifest: behavior,
        executable: bytes,
    }};
    assert_eq!(
        restarted.recover_pinned_workflow_actor(
            actor_id, "payments/compiled/restart", &resolver,
        ).unwrap(),
        actor_id,
    );
    assert!(restarted.actors[&actor_id].bytecode_module.is_some());
    assert_eq!(
        restarted.actors[&actor_id].get_state_field("counter").and_then(|v| v.as_int()),
        Some(7),
    );
    drop(restarted);
    std::fs::remove_dir_all(directory).unwrap();
}
