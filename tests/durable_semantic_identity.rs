use nulang::bytecode::CodeModule;
use nulang::content_identity::SemanticId;
use nulang::runtime::{ActorSnapshot, RecoveryIdentityPolicy, Runtime};

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

fn module_with_definition_identity(name: &str, semantic_id: Option<SemanticId>) -> CodeModule {
    let mut module = CodeModule::new(name);
    if let Some(id) = semantic_id {
        module.actor_semantic_ids.push(id);
    }
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
        module_with_definition_identity("matching", Some(id)),
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
        module_with_definition_identity("new-code", Some(semantic(b"new"))),
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
        module_with_definition_identity("legacy-module", None),
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
        module_with_definition_identity("identified", Some(semantic(b"current"))),
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
        module_with_definition_identity("identified", Some(semantic(b"current"))),
        vec![],
        vec![],
    );
    assert_eq!(compatible.recover_actor(actor_id), Some(actor_id));
}

#[test]
fn whole_program_identity_does_not_gate_definition_recovery() {
    let actor_id = 410_005;
    let definition_id = semantic(b"stable-definition");
    let mut module = module_with_definition_identity("Counter", Some(definition_id));
    module.semantic_id = Some(semantic(b"new-whole-program"));

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot(actor_id, Some(definition_id)))
        .unwrap();
    runtime.register_recovery_module(actor_id, module, vec![], vec![]);

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        Some(actor_id)
    );
    assert_eq!(
        runtime.actors[&actor_id].definition_semantic_id,
        Some(definition_id)
    );
}

#[test]
fn legacy_compatible_recovery_does_not_upgrade_provenance() {
    let actor_id = 410_006;
    let current_definition = semantic(b"current-definition");
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot(actor_id, None))
        .unwrap();
    runtime.register_recovery_module(
        actor_id,
        module_with_definition_identity("Counter", Some(current_definition)),
        vec![],
        vec![],
    );

    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));
    assert_eq!(runtime.actors[&actor_id].definition_semantic_id, None);
}

#[test]
fn malformed_artifact_identity_fails_closed_before_recovery() {
    let actor_id = 410_008;
    let definition_id = semantic(b"definition");
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            semantic_id: Some(definition_id.to_string()),
            artifact_id: Some("not-an-artifact-id".to_string()),
            ..ActorSnapshot::default()
        })
        .unwrap();
    runtime.register_recovery_module(
        actor_id,
        module_with_definition_identity("Counter", Some(definition_id)),
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
fn legacy_nbc_transport_rejects_self_asserted_semantic_identity() {
    let actor_id = 410_007;
    let id = semantic(b"transported");
    let module = module_with_definition_identity("transported", Some(id));
    let nbc = module.to_nbc(None).expect("encode NBC v1");
    let snapshot_json = serde_json::to_vec(&snapshot(actor_id, Some(id))).unwrap();

    let mut runtime = Runtime::new();
    assert!(!runtime.receive_migrated_actor(actor_id, nbc, snapshot_json));
    assert!(!runtime.actors.contains_key(&actor_id));
}
