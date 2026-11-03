use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::artifact_store::FileArtifactStore;
use nulang::bytecode::{ActorMeta, CodeModule};
use nulang::content_identity::SemanticId;
use nulang::runtime_artifact_manifest::RuntimeArtifactManifest;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        let i = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nulang-retained-runtime-{}-{i}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn identified_module() -> (
    CodeModule,
    ArtifactIdentityManifest,
    RuntimeArtifactManifest,
    SemanticId,
) {
    let program_id = SemanticId::from_canonical_bytes(b"runtime-manifest-program", []);
    let definition_id = SemanticId::from_canonical_bytes(b"counter-definition", []);
    let identity = ArtifactIdentityManifest::new(
        None,
        program_id,
        "nulang-test",
        "portable",
        "nulang-abi-v1",
        "bytecode",
        ["opt=0"],
    );
    let mut module = CodeModule::new("counter-module");
    module.semantic_id = Some(program_id);
    module.attach_artifact_identity(&identity).unwrap();
    let mut meta = ActorMeta::new("Counter");
    meta.persistent = true;
    module.actor_metadata.push(meta);
    module.actor_semantic_ids.push(definition_id);
    let runtime_manifest = RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
    (module, identity, runtime_manifest, definition_id)
}

#[test]
fn retained_runtime_manifest_restores_exact_definition_identity() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (module, identity, manifest, definition_id) = identified_module();
    store
        .retain_runtime(&identity, &manifest, &module.to_nbc(None).unwrap())
        .unwrap();
    let rehydrated = store
        .load_identified_module(identity.artifact_id())
        .unwrap();
    assert_eq!(rehydrated.name, module.name);
    assert_eq!(rehydrated.semantic_id, Some(identity.semantic_id()));
    assert_eq!(rehydrated.artifact_id, Some(identity.artifact_id()));
    assert_eq!(rehydrated.actor_semantic_ids, vec![definition_id]);
}

#[test]
fn executable_without_runtime_manifest_cannot_claim_historical_identity() {
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (module, identity, _manifest, _definition_id) = identified_module();
    store
        .retain(&identity, &module.to_nbc(None).unwrap())
        .unwrap();
    assert!(store
        .load_identified_module(identity.artifact_id())
        .is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_runtime_manifest_cannot_be_loaded_as_verified_history() {
    use std::os::unix::fs::symlink;
    let dir = TestDir::new();
    let store = FileArtifactStore::new(&dir.0);
    let (module, identity, manifest, _) = identified_module();
    store
        .retain_runtime(&identity, &manifest, &module.to_nbc(None).unwrap())
        .unwrap();
    let id_hex = identity.artifact_id().to_hex();
    let sidecar = dir
        .0
        .join(&id_hex[..2])
        .join(format!("{id_hex}.runtime.narm"));
    let outside = dir.0.join("external-sidecar.narm");
    std::fs::rename(&sidecar, &outside).unwrap();
    symlink(&outside, &sidecar).unwrap();
    assert!(store.load_runtime_manifest(identity.artifact_id()).is_err());
    assert!(store.retain_runtime_manifest(&manifest).is_err());
}
