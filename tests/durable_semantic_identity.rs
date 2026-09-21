use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::bytecode::CodeModule;
use nulang::content_identity::{ArtifactId, SemanticId};
use nulang::runtime::{ActorSnapshot, RecoveryIdentityPolicy, Runtime};

fn semantic(label: &[u8]) -> SemanticId {
    SemanticId::from_canonical_bytes(label, [])
}

fn snapshot(actor_id: u64, semantic_id: Option<SemanticId>) -> ActorSnapshot {
    snapshot_with_artifact(actor_id, semantic_id, None)
}

fn snapshot_with_artifact(
    actor_id: u64,
    semantic_id: Option<SemanticId>,
    artifact_id: Option<ArtifactId>,
) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        semantic_id: semantic_id.map(|id| id.to_string()),
        artifact_id: artifact_id.map(|id| id.to_string()),
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
fn identified_artifact_snapshot_rejects_different_executable_with_same_definition() {
    let actor_id = 410_007;
    let definition_id = semantic(b"stable-definition");
    let program_id = semantic(b"stable-program");

    let mut first = module_with_definition_identity("Counter", Some(definition_id));
    first.semantic_id = Some(program_id);
    let first_manifest = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );
    first.attach_artifact_identity(&first_manifest).unwrap();

    let mut changed_codegen = module_with_definition_identity("Counter", Some(definition_id));
    changed_codegen.semantic_id = Some(program_id);
    let changed_manifest = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=3"],
    );
    changed_codegen
        .attach_artifact_identity(&changed_manifest)
        .unwrap();
    assert_ne!(
        first_manifest.artifact_id(),
        changed_manifest.artifact_id(),
        "codegen identity must distinguish exact executables"
    );

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot_with_artifact(
            actor_id,
            Some(definition_id),
            Some(first_manifest.artifact_id()),
        ))
        .unwrap();
    runtime.register_recovery_module(actor_id, changed_codegen, vec![], vec![]);

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        None
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn matching_artifact_snapshot_recovers_with_exact_executable_provenance() {
    let actor_id = 410_008;
    let definition_id = semantic(b"definition");
    let program_id = semantic(b"program");
    let mut module = module_with_definition_identity("Counter", Some(definition_id));
    module.semantic_id = Some(program_id);
    let manifest = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );
    module.attach_artifact_identity(&manifest).unwrap();

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(snapshot_with_artifact(
            actor_id,
            Some(definition_id),
            Some(manifest.artifact_id()),
        ))
        .unwrap();
    runtime.register_recovery_module(actor_id, module, vec![], vec![]);

    assert_eq!(
        runtime.recover_actor_with_identity_policy(actor_id, RecoveryIdentityPolicy::Strict),
        Some(actor_id)
    );
    assert_eq!(
        runtime.actors[&actor_id].execution_artifact_id,
        Some(manifest.artifact_id())
    );
}

#[test]
fn legacy_snapshot_without_artifact_identity_is_not_silently_upgraded() {
    let actor_id = 410_009;
    let definition_id = semantic(b"definition");
    let program_id = semantic(b"program");
    let mut module = module_with_definition_identity("Counter", Some(definition_id));
    module.semantic_id = Some(program_id);
    let manifest = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );
    module.attach_artifact_identity(&manifest).unwrap();

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
    assert_eq!(runtime.actors[&actor_id].execution_artifact_id, None);
}

#[test]
fn legacy_nbc_transport_rejects_self_asserted_semantic_identity() {
    let actor_id = 410_010;
    let id = semantic(b"transported");
    let module = module_with_definition_identity("transported", Some(id));
    let nbc = module.to_nbc(None).expect("encode NBC v1");
    let snapshot_json = serde_json::to_vec(&snapshot(actor_id, Some(id))).unwrap();

    let mut runtime = Runtime::new();
    assert!(!runtime.receive_migrated_actor(actor_id, nbc, snapshot_json));
    assert!(!runtime.actors.contains_key(&actor_id));
}
