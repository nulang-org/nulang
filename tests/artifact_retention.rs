use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::bytecode::CodeModule;
use nulang::compiler_identity::{
    BYTECODE_ARTIFACT_ABI, BYTECODE_ARTIFACT_BACKEND, BYTECODE_ARTIFACT_TARGET,
    BYTECODE_COMPILER_VERSION,
};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    ActorSnapshot, JsonFileStore, PersistenceStore, RecoveryIdentityError,
    RecoveryIdentityPolicy, RetainedArtifact, Runtime,
};

fn proven_module(label: &str) -> CodeModule {
    let semantic_id = SemanticId::from_canonical_bytes(label.as_bytes(), []);
    let manifest = ArtifactIdentityManifest::new(
        None,
        semantic_id,
        BYTECODE_COMPILER_VERSION,
        BYTECODE_ARTIFACT_TARGET,
        BYTECODE_ARTIFACT_ABI,
        BYTECODE_ARTIFACT_BACKEND,
        std::iter::empty::<&str>(),
    );
    let mut module = CodeModule::new(label);
    module.semantic_id = Some(semantic_id);
    module.artifact_identity = Some(manifest);
    module
}

fn pinned_snapshot(actor_id: u64, module: &CodeModule) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        semantic_id: module.semantic_id.map(|id| id.to_string()),
        artifact_id: module.artifact_id().map(|id| id.to_string()),
        ..ActorSnapshot::default()
    }
}

#[test]
fn recovery_loads_exact_retained_artifact_when_current_code_changed() {
    let actor_id = 420_001;
    let old = proven_module("old-semantics");
    let old_artifact_id = old.artifact_id().unwrap();
    let new = proven_module("new-semantics");
    assert_ne!(old_artifact_id, new.artifact_id().unwrap());

    let mut runtime = Runtime::new();
    runtime.register_recovery_module(actor_id, old.clone(), vec![], vec![]);
    runtime
        .persistence
        .save_snapshot(pinned_snapshot(actor_id, &old))
        .unwrap();

    // Simulate deployment of a newer executable after the durable checkpoint.
    runtime.register_recovery_module(actor_id, new, vec![], vec![]);

    assert_eq!(
        runtime
            .recover_actor_checked(actor_id, RecoveryIdentityPolicy::Strict)
            .unwrap(),
        actor_id
    );
    assert_eq!(
        runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.bytecode_module.as_ref())
            .and_then(CodeModule::artifact_id),
        Some(old_artifact_id)
    );
}

#[test]
fn missing_historical_artifact_is_machine_readable_and_never_uses_current_code() {
    let actor_id = 420_002;
    let old = proven_module("missing-old");
    let old_artifact_id = old.artifact_id().unwrap();
    let new = proven_module("available-new");

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(pinned_snapshot(actor_id, &old))
        .unwrap();
    // Only the new artifact is registered/retained.
    runtime.register_recovery_module(actor_id, new, vec![], vec![]);

    let error = runtime.prepare_recovery_artifact(actor_id).unwrap_err();
    assert_eq!(
        error,
        RecoveryIdentityError::MissingHistoricalArtifact {
            actor_id,
            artifact_id: old_artifact_id,
        }
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn json_store_retains_exact_artifact_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "nulang-artifact-retention-{}-{}",
        std::process::id(),
        420_003u64
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let module = proven_module("json-retained");
    let artifact_id = module.artifact_id().unwrap();
    {
        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .save_artifact(RetainedArtifact::from_proven_module(&module).unwrap())
            .unwrap();
    }

    let reopened = JsonFileStore::new(&dir).unwrap();
    let retained = reopened.load_artifact(artifact_id).unwrap().unwrap();
    let restored = retained.restore_module(artifact_id).unwrap();
    assert_eq!(restored.artifact_id(), Some(artifact_id));
    assert_eq!(restored.semantic_id, module.semantic_id);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn retained_artifact_detects_byte_corruption() {
    let module = proven_module("corruption");
    let artifact_id = module.artifact_id().unwrap();
    let mut retained = RetainedArtifact::from_proven_module(&module).unwrap();
    retained.nbc_bytes.push(0xff);

    let error = retained.restore_module(artifact_id).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}
