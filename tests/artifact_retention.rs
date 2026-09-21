use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::bytecode::{ActorMeta, CodeModule};
use nulang::compiler_identity::{
    BYTECODE_ARTIFACT_ABI, BYTECODE_ARTIFACT_BACKEND, BYTECODE_ARTIFACT_TARGET,
    BYTECODE_COMPILER_VERSION,
};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    ActorSnapshot, JsonFileStore, PersistenceStore, RecoveryIdentityError, RecoveryIdentityPolicy,
    RetainedArtifact, Runtime,
};

fn semantic(label: &[u8]) -> SemanticId {
    SemanticId::from_canonical_bytes(label, [])
}

fn proven_module(program_label: &[u8], definition_id: SemanticId) -> CodeModule {
    let program_id = semantic(program_label);
    let manifest = ArtifactIdentityManifest::new(
        None,
        program_id,
        BYTECODE_COMPILER_VERSION,
        BYTECODE_ARTIFACT_TARGET,
        BYTECODE_ARTIFACT_ABI,
        BYTECODE_ARTIFACT_BACKEND,
        std::iter::empty::<&str>(),
    );
    let mut module = CodeModule::new(String::from_utf8_lossy(program_label));
    let mut meta = ActorMeta::new("Counter");
    meta.persistent = true;
    module.actor_metadata.push(meta);
    module.semantic_id = Some(program_id);
    module.actor_semantic_ids.push(definition_id);
    module.artifact_identity = Some(manifest);
    module
}

fn pinned_snapshot(
    actor_id: u64,
    definition_id: SemanticId,
    module: &CodeModule,
) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        semantic_id: Some(definition_id.to_string()),
        artifact_id: module.artifact_id().map(|id| id.to_string()),
        ..ActorSnapshot::default()
    }
}

#[test]
fn recovery_loads_exact_retained_artifact_after_deployment_changes() {
    let actor_id = 420_101;
    let old_definition = semantic(b"counter-v1");
    let new_definition = semantic(b"counter-v2");
    let old = proven_module(b"old-program", old_definition);
    let new = proven_module(b"new-program", new_definition);
    let old_artifact_id = old.artifact_id().unwrap();
    assert_ne!(old_artifact_id, new.artifact_id().unwrap());

    let mut runtime = Runtime::new();
    runtime.register_recovery_module(actor_id, old.clone(), vec![], vec![]);
    runtime
        .persistence
        .save_snapshot(pinned_snapshot(actor_id, old_definition, &old))
        .unwrap();

    // Simulate a deployment replacing the currently registered recovery code.
    runtime.register_recovery_module(actor_id, new, vec![], vec![]);

    assert_eq!(
        runtime
            .recover_actor_checked(actor_id, RecoveryIdentityPolicy::Strict)
            .unwrap(),
        actor_id
    );
    let actor = &runtime.actors[&actor_id];
    assert_eq!(actor.definition_semantic_id, Some(old_definition));
    assert_eq!(
        actor
            .bytecode_module
            .as_ref()
            .and_then(CodeModule::artifact_id),
        Some(old_artifact_id)
    );
}

#[test]
fn missing_historical_artifact_is_machine_readable_and_never_uses_current_code() {
    let actor_id = 420_102;
    let old_definition = semantic(b"missing-counter-v1");
    let new_definition = semantic(b"available-counter-v2");
    let old = proven_module(b"missing-old-program", old_definition);
    let new = proven_module(b"available-new-program", new_definition);
    let old_artifact_id = old.artifact_id().unwrap();

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(pinned_snapshot(actor_id, old_definition, &old))
        .unwrap();
    runtime.register_recovery_module(actor_id, new, vec![], vec![]);

    assert_eq!(
        runtime.prepare_recovery_artifact(actor_id).unwrap_err(),
        RecoveryIdentityError::MissingHistoricalArtifact {
            actor_id,
            artifact_id: old_artifact_id,
        }
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn retained_artifact_detects_sidecar_or_nbc_corruption() {
    let definition_id = semantic(b"corruption-definition");
    let module = proven_module(b"corruption-program", definition_id);
    let artifact_id = module.artifact_id().unwrap();
    let mut retained = RetainedArtifact::from_proven_module(&module).unwrap();

    retained.nbc_bytes.push(0xff);

    let error = retained.restore_module(artifact_id).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn json_store_reopens_exact_artifact_with_definition_sidecars() {
    let dir = std::env::temp_dir().join(format!(
        "nulang-artifact-retention-v2-{}-{}",
        std::process::id(),
        420_103u64
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let definition_id = semantic(b"json-definition");
    let module = proven_module(b"json-program", definition_id);
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
    assert_eq!(restored.actor_semantic_ids, vec![definition_id]);

    let _ = std::fs::remove_dir_all(&dir);
}
