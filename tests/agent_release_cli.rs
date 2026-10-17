use nulang::agent_release::{asset_digest, AgentReleaseDefinition, AgentReleaseEnvelope, ModelBinding};
use std::path::Path;
use std::process::Command;

fn cli(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(args)
        .arg(path)
        .output()
        .expect("launch Nulang CLI")
}

#[test]
fn release_cli_seals_and_verifies_a_pinned_release() {
    let temp = std::env::temp_dir().join(format!(
        "nulang_release_cli_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&temp).unwrap();
    let definition = AgentReleaseDefinition {
        schema: "nulang.agent-release/v1".into(),
        agent: "research-agent".into(),
        version: "1.0.0".into(),
        artifact_id: "a".repeat(64),
        prompt: asset_digest(b"prompt"),
        policy: asset_digest(b"policy"),
        model: ModelBinding {
            provider: "mistral".into(),
            model: "large".into(),
            revision: "2026-10-17".into(),
        },
        skills: vec![],
        tools: vec![],
    };
    let definition_path = temp.join("definition.json");
    let envelope_path = temp.join("sealed.json");
    std::fs::write(&definition_path, serde_json::to_vec(&definition).unwrap()).unwrap();

    let sealed = cli(&definition_path, &["nula", "release", "seal"]);
    assert!(sealed.status.success(), "{}", String::from_utf8_lossy(&sealed.stderr));
    let envelope = AgentReleaseEnvelope::parse_and_verify(&sealed.stdout).unwrap();
    assert_eq!(envelope.definition.agent, "research-agent");
    std::fs::write(&envelope_path, &sealed.stdout).unwrap();

    let verified = cli(&envelope_path, &["nula", "release", "verify"]);
    assert!(verified.status.success(), "{}", String::from_utf8_lossy(&verified.stderr));
    let output: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(output["command"], "release.verify");
    assert_eq!(output["release_id"], envelope.release_id);

    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn release_cli_rejects_invalid_and_oversized_input_without_stdout() {
    let temp = std::env::temp_dir().join(format!("nulang_release_invalid_{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();
    let malformed = temp.join("malformed.json");
    std::fs::write(&malformed, b"{").unwrap();
    let result = cli(&malformed, &["nula", "release", "seal"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());

    let too_large = temp.join("large.json");
    std::fs::write(&too_large, vec![b'a'; 65 * 1024]).unwrap();
    let result = cli(&too_large, &["nula", "release", "verify"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    std::fs::remove_dir_all(temp).unwrap();
}
