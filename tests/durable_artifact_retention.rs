use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::artifact_store::{ArtifactStore, LocalArtifactStore};
use nulang::bytecode::{ActorMeta, CodeModule, Instruction, OpCode};
use nulang::content_identity::{SemanticId, SourceId};
use nulang::runtime::{ActorSnapshot, RecoveryIdentityPolicy, Runtime};

fn temp_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "nulang-durable-artifact-retention-{}",
        std::process::id()
    ))
}

#[test]
fn retained_artifact_rehydrates_identity_and_strictly_recovers() {
    let root = temp_root();
    let _ = std::fs::remove_dir_all(&root);
    let store = LocalArtifactStore::new(&root);

    let program_semantic = SemanticId::from_canonical_bytes(b"program-v1", []);
    let definition_semantic = SemanticId::from_canonical_bytes(b"Counter-v1", []);

    let mut module = CodeModule::new("retained-runtime");
    module.semantic_id = Some(program_semantic);
    module.actor_metadata.push(ActorMeta::new("Counter"));
    module.actor_semantic_ids.push(definition_semantic);
    module.emit(Instruction::new0(OpCode::Halt));

    let manifest = ArtifactIdentityManifest::new(
        Some(SourceId::from_bytes(b"actor Counter {}")),
        program_semantic,
        "nulangc-test",
        "nulang-vm-v1",
        "nbc-v1",
        "bytecode",
        ["opt=0"],
    );
    let artifact_id = manifest.artifact_id();
    module.artifact_id = Some(artifact_id);

    assert_eq!(store.retain(&manifest, &module, None).unwrap(), artifact_id);
    let retained = store.require(artifact_id).unwrap();

    assert_eq!(retained.module.semantic_id, Some(program_semantic));
    assert_eq!(retained.module.artifact_id, Some(artifact_id));
    assert_eq!(
        retained.module.actor_semantic_ids,
        vec![definition_semantic]
    );

    let actor_id = 440_001;
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            semantic_id: Some(definition_semantic.to_string()),
            artifact_id: Some(artifact_id.to_string()),
            ..ActorSnapshot::default()
        })
        .unwrap();
    runtime.register_recovery_module(actor_id, retained.module, vec![], vec![]);

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        Some(actor_id)
    );
    let actor = runtime.actors.get(&actor_id).expect("recovered actor");
    assert_eq!(actor.definition_semantic_id, Some(definition_semantic));
    assert_eq!(actor.artifact_id, Some(artifact_id));

    let _ = std::fs::remove_dir_all(root);
}
