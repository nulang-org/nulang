use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::artifact_store::FileArtifactStore;
use nulang::ast::StateModel as AstStateModel;
use nulang::bytecode::{ActorMeta, CodeModule, Constant};
use nulang::content_identity::SemanticId;
use nulang::runtime::{ActorSnapshot, RecoveryIdentityPolicy, Runtime};
use nulang::runtime_artifact_manifest::RuntimeArtifactManifest;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct TestDir(std::path::PathBuf);

impl TestDir {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nulang-artifact-recovery-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn semantic(label: &[u8]) -> SemanticId {
    SemanticId::from_canonical_bytes(label, [])
}

fn retained_mixed_module(
    store: &FileArtifactStore,
) -> (
    ArtifactIdentityManifest,
    SemanticId,
    SemanticId,
) {
    let program_id = semantic(b"mixed-program");
    let workflow_id = semantic(b"workflow-definition");
    let counter_id = semantic(b"counter-definition");
    let identity = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );

    let mut module = CodeModule::new("mixed-module");
    module.semantic_id = Some(program_id);
    module.artifact_id = Some(identity.artifact_id());

    // Put a workflow first so recovery that inspects "any metadata in the
    // module" would incorrectly classify the Counter snapshot as a workflow.
    let mut workflow = ActorMeta::new("WorkflowOnly");
    workflow.persistent = true;
    workflow.is_workflow = true;
    workflow.state_models = vec![("workflow_only".into(), AstStateModel::Local)];
    workflow.state_defaults = vec![("workflow_only".into(), Constant::Int(99))];
    module.actor_metadata.push(workflow);

    let mut counter = ActorMeta::new("Counter");
    counter.persistent = true;
    counter.state_models = vec![("counter_local".into(), AstStateModel::Local)];
    counter.state_defaults = vec![("counter_local".into(), Constant::Int(7))];
    module.actor_metadata.push(counter);

    module.actor_semantic_ids = vec![workflow_id, counter_id];

    let runtime_manifest = RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
    let bytes = module.to_nbc(None).unwrap();
    store
        .retain_runtime(&identity, &runtime_manifest, &bytes)
        .unwrap();

    (identity, workflow_id, counter_id)
}

fn identified_snapshot(
    actor_id: u64,
    definition_id: SemanticId,
    identity: &ArtifactIdentityManifest,
) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        semantic_id: Some(definition_id.to_string()),
        artifact_id: Some(identity.artifact_id().to_string()),
        ..ActorSnapshot::default()
    }
}

#[test]
fn historical_artifact_recovery_selects_exact_definition_metadata() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (identity, _workflow_id, counter_id) = retained_mixed_module(&store);
    let actor_id = 420_001;

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(identified_snapshot(actor_id, counter_id, &identity))
        .unwrap();

    assert_eq!(
        runtime
            .recover_actor_from_artifact_store(
                actor_id,
                &store,
                RecoveryIdentityPolicy::Strict,
            )
            .unwrap(),
        actor_id
    );

    let actor = runtime.actors.get(&actor_id).unwrap();
    assert_eq!(actor.name, "Counter");
    assert!(!actor.is_workflow);
    assert!(!actor.is_agent);
    assert_eq!(actor.definition_semantic_id, Some(counter_id));
    assert_eq!(actor.execution_artifact_id, Some(identity.artifact_id()));
    assert_eq!(
        actor
            .get_state_field("counter_local")
            .and_then(|value| value.as_int()),
        Some(7)
    );
    assert_eq!(actor.get_state_field("workflow_only"), None);
    assert!(actor.state_models.contains_key("counter_local"));
    assert!(!actor.state_models.contains_key("workflow_only"));

    let module = actor.bytecode_module.as_ref().unwrap();
    assert_eq!(module.artifact_id, Some(identity.artifact_id()));
    assert_eq!(module.actor_semantic_ids.len(), 2);
}

#[test]
fn historical_artifact_recovery_requires_runtime_sidecar() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let program_id = semantic(b"program");
    let definition_id = semantic(b"counter");
    let identity = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulangc-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );

    let mut module = CodeModule::new("counter-module");
    module.semantic_id = Some(program_id);
    module.artifact_id = Some(identity.artifact_id());
    module.actor_metadata.push(ActorMeta::new("Counter"));
    module.actor_semantic_ids.push(definition_id);
    store.retain(&identity, &module.to_nbc(None).unwrap()).unwrap();

    let actor_id = 420_002;
    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(identified_snapshot(actor_id, definition_id, &identity))
        .unwrap();

    let error = runtime
        .recover_actor_from_artifact_store(actor_id, &store, RecoveryIdentityPolicy::Strict)
        .unwrap_err();
    assert!(
        error.to_string().contains("runtime manifest"),
        "unexpected error: {error}"
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn historical_artifact_recovery_rejects_unknown_definition_identity() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (identity, _workflow_id, _counter_id) = retained_mixed_module(&store);
    let actor_id = 420_003;
    let unknown = semantic(b"not-in-retained-artifact");

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(identified_snapshot(actor_id, unknown, &identity))
        .unwrap();

    let error = runtime
        .recover_actor_from_artifact_store(actor_id, &store, RecoveryIdentityPolicy::Strict)
        .unwrap_err();
    assert!(
        error.to_string().contains("no actor definition matching"),
        "unexpected error: {error}"
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn historical_artifact_recovery_rejects_legacy_snapshot_without_artifact_id() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (_identity, _workflow_id, counter_id) = retained_mixed_module(&store);
    let actor_id = 420_004;

    let mut runtime = Runtime::new();
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            semantic_id: Some(counter_id.to_string()),
            ..ActorSnapshot::default()
        })
        .unwrap();

    let error = runtime
        .recover_actor_from_artifact_store(
            actor_id,
            &store,
            RecoveryIdentityPolicy::LegacyCompatible,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("no ArtifactId"),
        "unexpected error: {error}"
    );
    assert!(!runtime.actors.contains_key(&actor_id));
}
