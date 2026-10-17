use nulang::agent_release::{
    asset_digest, AgentReleaseDefinition, AgentReleaseEnvelope, ModelBinding, NamedAsset,
    RELEASE_SCHEMA,
};

fn fixture() -> AgentReleaseDefinition {
    AgentReleaseDefinition {
        schema: RELEASE_SCHEMA.to_string(),
        agent: "research-agent".into(),
        version: "1.0.0".into(),
        artifact_id: "a".repeat(64),
        prompt: asset_digest(b"system prompt v1"),
        policy: asset_digest(b"deny all writes"),
        model: ModelBinding {
            provider: "mistral".into(),
            model: "large".into(),
            revision: "2026-10-17".into(),
        },
        skills: vec![
            NamedAsset {
                name: "z-summary".into(),
                digest: asset_digest(b"summary"),
            },
            NamedAsset {
                name: "a-research".into(),
                digest: asset_digest(b"research"),
            },
        ],
        tools: vec![NamedAsset {
            name: "web-search".into(),
            digest: asset_digest(b"tool schema"),
        }],
    }
}

#[test]
fn seal_is_deterministic_and_roundtrips_without_secret_fields() {
    let sealed = AgentReleaseEnvelope::seal(fixture()).unwrap();
    let bytes = serde_json::to_vec(&sealed).unwrap();
    let decoded = AgentReleaseEnvelope::parse_and_verify(&bytes).unwrap();
    assert_eq!(sealed, decoded);
    assert!(sealed.release_id.starts_with("blake3:"));
    assert_eq!(sealed.release_id.len(), 71);
    assert_eq!(sealed.definition.skills[0].name, "a-research");
    assert_eq!(
        sealed.release_id,
        AgentReleaseEnvelope::seal(fixture()).unwrap().release_id
    );
}

#[test]
fn tampering_with_any_bound_asset_or_model_is_rejected() {
    let sealed = AgentReleaseEnvelope::seal(fixture()).unwrap();
    let mut mutated = serde_json::to_value(&sealed).unwrap();
    mutated["definition"]["model"]["revision"] = serde_json::json!("other");
    assert!(
        AgentReleaseEnvelope::parse_and_verify(&serde_json::to_vec(&mutated).unwrap()).is_err()
    );

    let mut mutated = serde_json::to_value(&sealed).unwrap();
    mutated["definition"]["policy"] = serde_json::json!(asset_digest(b"allow writes"));
    assert!(
        AgentReleaseEnvelope::parse_and_verify(&serde_json::to_vec(&mutated).unwrap()).is_err()
    );
}

#[test]
fn malformed_inputs_fail_closed() {
    let mut def = fixture();
    def.skills.push(def.skills[1].clone());
    assert!(AgentReleaseEnvelope::seal(def).is_err());

    let mut def = fixture();
    def.policy = "sha256:123".into();
    assert!(AgentReleaseEnvelope::seal(def).is_err());

    let mut def = fixture();
    def.model.revision = String::new();
    assert!(AgentReleaseEnvelope::seal(def).is_err());

    let mut def = fixture();
    def.schema = "nulang.agent-release/v2".into();
    assert!(AgentReleaseEnvelope::seal(def).is_err());

    let sealed = AgentReleaseEnvelope::seal(fixture()).unwrap();
    let mut unknown = serde_json::to_value(&sealed).unwrap();
    unknown["definition"]["provider_api_key"] = serde_json::json!("secret");
    assert!(
        AgentReleaseEnvelope::parse_and_verify(&serde_json::to_vec(&unknown).unwrap()).is_err()
    );

    let too_big = vec![b' '; 65 * 1024];
    assert!(AgentReleaseEnvelope::parse_and_verify(&too_big).is_err());
}

#[test]
fn bound_bytes_are_verified_not_just_the_declared_release_id() {
    let digest = asset_digest(b"policy-v1");
    assert!(nulang::agent_release::verify_asset(&digest, b"policy-v1").is_ok());
    assert!(nulang::agent_release::verify_asset(&digest, b"policy-v2").is_err());
}
