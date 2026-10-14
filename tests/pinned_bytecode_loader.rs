//! The executable resumed by the runtime must be decoded from *the exact*
//! immutable bytes recorded in the original durable execution pin.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorActor, BehaviorArtifact, BehaviorManifest, BehaviorPackage, BehaviorPersistence,
    MigrationIdentityCoverage, BEHAVIOR_ARTIFACT_KIND_NBC_V1, BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::bytecode::{ActorMeta, BehaviorTableEntry, CodeModule, Instruction};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    commit_pinned_workflow_start, ActorSnapshot, ImmutablePinnedArtifactResolver,
    MemoryStore, PinnedArtifactBundle, Runtime,
};
use std::io::{self, ErrorKind};

fn fixture_module(schema: &str) -> CodeModule {
    let mut module = CodeModule::new("verified-payment-module");
    module.instructions.push(Instruction::decode(0x57000000).unwrap());
    module.behaviors.push(BehaviorTableEntry {
        name: format!("{schema}.step"),
        param_count: 0,
        code_offset: 0,
        local_count: 1,
        effect_mask: 0,
        compensate_offset: None,
        content_hash: None,
        source_location: None,
        parallel_branches: None,
    });
    let mut meta = ActorMeta::new(schema);
    meta.persistent = true;
    meta.is_workflow = true;
    meta.behavior_indices = vec![0];
    module.add_actor_meta(meta);
    module
}

fn manifest(bytes: &[u8]) -> BehaviorManifest {
    let identity = ArtifactIdentityManifest::new(
        None,
        SemanticId::from_canonical_bytes(b"verified-payment", []),
        "compiler-1",
        "nulang-bytecode-v1",
        "nulang-abi-v1",
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

struct Resolver {
    digest: String,
    bundle: PinnedArtifactBundle,
}

impl ImmutablePinnedArtifactResolver for Resolver {
    fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
        Ok((self.digest == digest).then(|| self.bundle.clone()))
    }
}

fn stored_execution(original_bytes: &[u8]) -> Runtime {
    let mut store = MemoryStore::new();
    commit_pinned_workflow_start(
        &mut store,
        501,
        "payments/501",
        "Payment",
        &manifest(original_bytes),
        original_bytes,
        ActorSnapshot {
            actor_id: 501,
            sequence: 1,
            activation_epoch: 1,
            schema_name: Some("Payment".into()),
            ..ActorSnapshot::default()
        },
        vec![],
    )
    .unwrap();
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(store);
    runtime
}

fn resolver(bytes: &[u8]) -> Resolver {
    Resolver {
        digest: manifest(bytes).artifact.digest.clone(),
        bundle: PinnedArtifactBundle {
            manifest: manifest(bytes),
            executable: bytes.to_vec(),
        },
    }
}

#[test]
fn recovery_loads_and_dispatches_only_module_decoded_from_pinned_nbc_bytes() {
    let original = fixture_module("Payment");
    let bytes = original.to_nbc(None).unwrap();
    let mut runtime = stored_execution(&bytes);

    assert_eq!(
        runtime
            .recover_pinned_workflow_actor(501, "payments/501", &resolver(&bytes))
            .unwrap(),
        501,
    );

    let actor = &runtime.actors[&501];
    let loaded = actor.bytecode_module.as_ref().expect("verified compiled module");
    assert_eq!(loaded.name, original.name);
    assert_eq!(loaded.behaviors.len(), original.behaviors.len());
    assert_eq!(loaded.instructions.len(), original.instructions.len());
    assert_eq!(actor.bytecode_offsets, vec![0]);
}

#[test]
fn different_valid_nbc_is_rejected_before_actor_or_module_registration() {
    let bytes = fixture_module("Payment").to_nbc(None).unwrap();
    let replacement = fixture_module("WrongPayment").to_nbc(None).unwrap();
    let mut runtime = stored_execution(&bytes);
    let error = runtime
        .recover_pinned_workflow_actor(
            501, "payments/501",
            &Resolver {
                digest: manifest(&bytes).artifact.digest,
                bundle: PinnedArtifactBundle {
                    manifest: manifest(&replacement),
                    executable: replacement,
                },
            },
        )
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(!runtime.actors.contains_key(&501));
}

#[test]
fn valid_digest_but_invalid_nbc_is_rejected_before_publication() {
    let invalid_bytes = b"this-is-not-an-nbc-module";
    let mut runtime = stored_execution(invalid_bytes);
    assert_eq!(
        runtime
            .recover_pinned_workflow_actor(501, "payments/501", &resolver(invalid_bytes))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&501));
}

#[test]
fn schema_mismatch_between_pinned_nbc_and_snapshot_fails_closed() {
    let bytes = fixture_module("Other").to_nbc(None).unwrap();
    let mut runtime = stored_execution(&bytes);
    assert_eq!(
        runtime
            .recover_pinned_workflow_actor(501, "payments/501", &resolver(&bytes))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&501));
}

#[test]
fn invalid_compiled_behavior_index_is_rejected_without_panicking() {
    let mut module = fixture_module("Payment");
    module.actor_metadata[0].behavior_indices = vec![999];
    let bytes = module.to_nbc(None).unwrap();
    let mut runtime = stored_execution(&bytes);
    assert_eq!(
        runtime
            .recover_pinned_workflow_actor(501, "payments/501", &resolver(&bytes))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&501));
}

#[test]
fn externally_registered_module_cannot_override_verified_bytecode_loader() {
    let bytes = fixture_module("Payment").to_nbc(None).unwrap();
    let mut runtime = stored_execution(&bytes);
    runtime.register_recovery_module(
        501,
        fixture_module("Unverified"),
        vec![0],
        vec![None],
    );
    assert_eq!(
        runtime
            .recover_pinned_workflow_actor(501, "payments/501", &resolver(&bytes))
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported,
    );
    assert!(!runtime.actors.contains_key(&501));
}

#[cfg(feature = "sqlite")]
#[test]
fn verified_pinned_bytecode_subprocess_helper() {
    use nulang::runtime::LibsqlStore;
    let Ok(mode) = std::env::var("NULANG_VERIFIED_NBC_CHILD_MODE") else {
        return;
    };
    let path = std::path::PathBuf::from(
        std::env::var_os("NULANG_VERIFIED_NBC_CHILD_DB").expect("child database path"),
    );
    let module = fixture_module("Payment");
    let bytes = module.to_nbc(None).unwrap();

    match mode.as_str() {
        "create" => {
            let mut store = LibsqlStore::new(&path).unwrap();
            commit_pinned_workflow_start(
                &mut store,
                502,
                "payments/502",
                "Payment",
                &manifest(&bytes),
                &bytes,
                ActorSnapshot {
                    actor_id: 502,
                    sequence: 1,
                    activation_epoch: 1,
                    schema_name: Some("Payment".into()),
                    ..ActorSnapshot::default()
                },
                vec![],
            )
            .unwrap();
        }
        "reject-substitute" | "resume" => {
            let mut runtime = Runtime::new();
            runtime.persistence = Box::new(LibsqlStore::new(&path).unwrap());
            assert_eq!(runtime.recover_actor(502), None);

            let provided = if mode == "reject-substitute" {
                fixture_module("Other").to_nbc(None).unwrap()
            } else {
                bytes.clone()
            };
            let selected = Resolver {
                digest: manifest(&bytes).artifact.digest,
                bundle: PinnedArtifactBundle {
                    manifest: manifest(&provided),
                    executable: provided,
                },
            };
            let outcome = runtime.recover_pinned_workflow_actor(
                502, "payments/502", &selected,
            );

            if mode == "reject-substitute" {
                assert_eq!(outcome.unwrap_err().kind(), ErrorKind::InvalidData);
                assert!(runtime.actors.is_empty());
            } else {
                assert_eq!(outcome.unwrap(), 502);
                let loaded = runtime.actors[&502].bytecode_module
                    .as_ref().expect("decoded pinned bytecode");
                assert_eq!(loaded.name, module.name);
                assert_eq!(runtime.actors[&502].bytecode_offsets, vec![0]);
            }
        }
        invalid => panic!("unexpected subprocess mode: {invalid}"),
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn verified_bytecode_loader_survives_real_process_restart() {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-verified-bytecode-restart-{}-{nonce}", std::process::id(),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("recovery.db");

    for mode in ["create", "reject-substitute", "resume"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "verified_pinned_bytecode_subprocess_helper"])
            .env("NULANG_VERIFIED_NBC_CHILD_MODE", mode)
            .env("NULANG_VERIFIED_NBC_CHILD_DB", &database)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{mode}: stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("1 passed"),
            "{mode} helper did not execute",
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
