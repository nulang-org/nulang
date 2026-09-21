use nulang::bytecode::CodeModule;
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    ActorSnapshot, PersistenceStore, RecoveryIdentityPolicy, Runtime,
};

fn semantic(label: &[u8]) -> SemanticId {
    SemanticId::from_canonical_bytes(label, [])
}

fn snapshot(actor_id: u64, semantic_id: Option<SemanticId>) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        semantic_id: semantic_id.map(|id| id.to_string()),
        ..ActorSnapshot::default()
    }
}

fn module_with_identity(name: &str, semantic_id: Option<SemanticId>) -> CodeModule {
    let mut module = CodeModule::new(name);
    module.semantic_id = semantic_id;
    module
}

#[test]
fn identified_snapshot_recovers_only_under_matching_code() {
    let id = semantic(b"same");
    let actor_id = 410_001;
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot(actor_id, Some(id)))
        .unwrap();
    runtime.register_recovery_module(
        actor_id,
        module_with_identity("matching", Some(id)),
        vec![],
        vec![],
    );

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        Some(actor_id)
    );
}

#[test]
fn identified_snapshot_rejects_semantically_different_code() {
    let actor_id = 410_002;
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot(actor_id, Some(semantic(b"old"))))
        .unwrap();
    runtime.register_recovery_module(
        actor_id,
        module_with_identity("new-code", Some(semantic(b"new"))),
        vec![],
        vec![],
    );

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        None
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn identified_snapshot_rejects_unidentified_recovery_module() {
    let actor_id = 410_003;
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot(actor_id, Some(semantic(b"identified"))))
        .unwrap();
    runtime.register_recovery_module(
        actor_id,
        module_with_identity("legacy-module", None),
        vec![],
        vec![],
    );

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        None
    );
}

#[test]
fn legacy_snapshot_requires_explicit_legacy_compatible_policy() {
    let actor_id = 410_004;
    let mut strict = Runtime::new();
    strict
        .persistence
        .save_snapshot(snapshot(actor_id, None))
        .unwrap();
    strict.register_recovery_module(
        actor_id,
        module_with_identity("identified", Some(semantic(b"current"))),
        vec![],
        vec![],
    );
    assert_eq!(
        strict.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        None
    );

    let mut compatible = Runtime::new();
    compatible
        .persistence
        .save_snapshot(snapshot(actor_id, None))
        .unwrap();
    compatible.register_recovery_module(
        actor_id,
        module_with_identity("identified", Some(semantic(b"current"))),
        vec![],
        vec![],
    );
    assert_eq!(compatible.recover_actor(actor_id), Some(actor_id));
}
