//! Fail-closed resume admission for one concrete durable execution.
//! These tests intentionally distinguish schema upgrade preflight from replay admission.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorActor, BehaviorArtifact, BehaviorManifest, BehaviorPackage, BehaviorPersistence,
    MigrationIdentityCoverage, BEHAVIOR_ARTIFACT_KIND_NBC_V1, BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::content_identity::SemanticId;
use nulang::execution_compatibility::{DurableExecutionPin, ExecutionPinError};

fn manifest(executable: &[u8]) -> BehaviorManifest {
    let identity = ArtifactIdentityManifest::new(
        None,
        SemanticId::from_canonical_bytes(b"durable-payments", []),
        "compiler-1",
        "nulang-bytecode-v1",
        "nulang-abi-v1",
        "bytecode",
        std::iter::empty::<&str>(),
    );
    BehaviorManifest {
        schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
        package: BehaviorPackage {
            name: "payments".to_string(),
            version: "1.0.0".to_string(),
            language_version: "1".to_string(),
        },
        artifact: BehaviorArtifact {
            kind: BEHAVIOR_ARTIFACT_KIND_NBC_V1.to_string(),
            digest: format!("blake3:{}", blake3::hash(executable).to_hex()),
            source_id: None,
            semantic_id: identity.semantic_id().to_string(),
            artifact_id: identity.artifact_id().to_string(),
            compiler_version: identity.compiler_version().to_string(),
            target: identity.target().to_string(),
            abi: identity.abi().to_string(),
            backend: identity.backend().to_string(),
            flags: Vec::new(),
        },
        actors: vec![BehaviorActor {
            name: "payments::PaymentProcess".to_string(),
            persistence: BehaviorPersistence::Durable,
            schema_version: 1,
            state_schema_semantic_id: SemanticId::from_canonical_bytes(
                b"payment-process-state-v1",
                [],
            )
            .to_string(),
            migration_identity: MigrationIdentityCoverage::TopologyOnly,
            migrations: Vec::new(),
        }],
    }
}

#[test]
fn exact_artifact_and_manifest_allow_resume_after_pin_roundtrip() {
    let bytes = b"stable-nbc-v1";
    let deployed = manifest(bytes);
    let pin = DurableExecutionPin::create("payments/run-42", &deployed, bytes).unwrap();
    let decoded = DurableExecutionPin::from_json(&pin.to_json().unwrap()).unwrap();
    assert_eq!(pin, decoded);
    decoded
        .validate_resume("payments/run-42", &deployed, bytes)
        .unwrap();
}

#[test]
fn different_executable_with_structurally_compatible_manifest_fails_closed() {
    let previous_bytes = b"payment-v1";
    let incoming_bytes = b"payment-v2";
    let previous = manifest(previous_bytes);
    let incoming = manifest(incoming_bytes);
    // Structural upgrade admission is not enough to authorize historical resume.
    incoming.validate_upgrade_from(&previous).unwrap();

    let pin = DurableExecutionPin::create("payments/42", &previous, previous_bytes).unwrap();
    assert!(matches!(
        pin.validate_resume("payments/42", &incoming, incoming_bytes),
        Err(ExecutionPinError::ExecutableDigestMismatch { .. })
    ));
}

#[test]
fn unchanged_executable_with_changed_manifest_semantics_fails_closed() {
    let executable = b"same-nbc";
    let original = manifest(executable);
    let mut incoming = original.clone();
    incoming.package.version = "2.0.0".to_string();
    let pin = DurableExecutionPin::create("payments/42", &original, executable).unwrap();

    assert!(matches!(
        pin.validate_resume("payments/42", &incoming, executable),
        Err(ExecutionPinError::BehaviorDigestMismatch { .. })
    ));
}

#[test]
fn candidate_sidecar_must_match_the_actual_executable() {
    let bytes = b"verified-executable";
    let deployed = manifest(bytes);
    let pin = DurableExecutionPin::create("payments/42", &deployed, bytes).unwrap();

    assert!(matches!(
        pin.validate_resume("payments/42", &deployed, b"tampered-executable"),
        Err(ExecutionPinError::Manifest(_))
    ));
    assert!(matches!(
        DurableExecutionPin::create("payments/43", &deployed, b"wrong-bytes"),
        Err(ExecutionPinError::Manifest(_))
    ));
}

#[test]
fn a_pin_cannot_be_reused_for_another_execution() {
    let bytes = b"artifact";
    let deployed = manifest(bytes);
    let pin = DurableExecutionPin::create("payments/42", &deployed, bytes).unwrap();
    assert!(matches!(
        pin.validate_resume("payments/43", &deployed, bytes),
        Err(ExecutionPinError::ExecutionIdMismatch { .. })
    ));
}

#[test]
fn absent_corrupt_and_future_pins_fail_closed() {
    assert!(DurableExecutionPin::from_json(br#"{}"#).is_err());
    let bytes = b"artifact";
    let deployed = manifest(bytes);
    let pin = DurableExecutionPin::create("payments/42", &deployed, bytes).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&pin.to_json().unwrap()).unwrap();

    value["version"] = serde_json::json!(2);
    assert!(matches!(
        DurableExecutionPin::from_json(&serde_json::to_vec(&value).unwrap()),
        Err(ExecutionPinError::UnsupportedVersion { version: 2 })
    ));

    value["version"] = serde_json::json!(1);
    value["executable_digest"] = serde_json::json!("not-a-digest");
    assert!(matches!(
        DurableExecutionPin::from_json(&serde_json::to_vec(&value).unwrap()),
        Err(ExecutionPinError::InvalidDigest { .. })
    ));
}

#[test]
fn cli_preflight_roundtrip_rejects_modified_artifact() {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "nulang-durable-execution-pin-{}-{}",
        std::process::id(),
        NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory).unwrap();
    let manifest_path = directory.join("payment.behavior.json");
    let artifact_path = directory.join("payment.nbc");
    let pin_path = directory.join("payment.pin.json");
    let bytes = b"executable-1";
    fs::write(&manifest_path, manifest(bytes).to_json().unwrap()).unwrap();
    fs::write(&artifact_path, bytes).unwrap();

    let executable = env!("CARGO_BIN_EXE_nulang-execution-preflight");
    let pin = Command::new(executable)
        .args([
            "pin",
            "payments/99",
            manifest_path.to_str().unwrap(),
            artifact_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(pin.status.success(), "{}", String::from_utf8_lossy(&pin.stderr));
    fs::write(&pin_path, pin.stdout).unwrap();

    let verify = || {
        Command::new(executable)
            .args([
                "verify",
                "payments/99",
                pin_path.to_str().unwrap(),
                manifest_path.to_str().unwrap(),
                artifact_path.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let success = verify();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );

    fs::write(&artifact_path, b"modified-binary").unwrap();
    let rejected = verify();
    assert!(!rejected.status.success());
    fs::remove_dir_all(directory).unwrap();
}
