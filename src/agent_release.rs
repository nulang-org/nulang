//! Versioned, content-addressed identity of an agent *release*.
//!
//! This contract binds declared model, program, prompt, skills, tools and
//! policy versions. It is not a signature, authorization decision, artifact
//! admission result, or a content store. A trusted caller must also verify
//! supplied asset bytes and pin the expected release ID.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const RELEASE_SCHEMA: &str = "nulang.agent-release/v1";
pub const MAX_RELEASE_BYTES: usize = 64 * 1024;
const DOMAIN: &[u8] = b"nulang.agent-release.v1\0";
const MAX_BINDINGS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    pub provider: String,
    pub model: String,
    /// An explicit provider revision, immutable model ID or pinned deployment revision.
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedAsset {
    pub name: String,
    /// BLAKE3 digest of the *exact* content bytes, not a mutable registry tag.
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReleaseDefinition {
    pub schema: String,
    pub agent: String,
    pub version: String,
    /// Compiler-produced artifact identity (not a permission grant).
    pub artifact_id: String,
    pub prompt: String,
    pub policy: String,
    pub model: ModelBinding,
    #[serde(default)]
    pub skills: Vec<NamedAsset>,
    #[serde(default)]
    pub tools: Vec<NamedAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReleaseEnvelope {
    pub definition: AgentReleaseDefinition,
    pub release_id: String,
}

#[derive(Debug)]
pub enum AgentReleaseError {
    Invalid(String),
    Json(serde_json::Error),
    TooLarge,
    IdentityMismatch,
    AssetMismatch,
}

impl std::fmt::Display for AgentReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid agent release: {message}"),
            Self::Json(error) => write!(f, "invalid agent release JSON: {error}"),
            Self::TooLarge => write!(f, "release manifest exceeds {MAX_RELEASE_BYTES} bytes"),
            Self::IdentityMismatch => write!(f, "release ID does not match its definition"),
            Self::AssetMismatch => write!(f, "asset content does not match its declared digest"),
        }
    }
}

impl std::error::Error for AgentReleaseError {}

impl From<serde_json::Error> for AgentReleaseError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Hash the *exact* bytes that an asset registry stores. Caller is responsible
/// for providing trusted bytes before dispatching any effects.
pub fn asset_digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Verify actual bytes against a bound asset identity, not merely its name.
pub fn verify_asset(expected: &str, actual: &[u8]) -> Result<(), AgentReleaseError> {
    validate_digest(expected)?;
    if expected != asset_digest(actual) {
        return Err(AgentReleaseError::AssetMismatch);
    }
    Ok(())
}

impl AgentReleaseEnvelope {
    /// Construct a canonical, reproducible identity from the complete
    /// declaration. Named assets are sorted before fingerprinting.
    pub fn seal(mut definition: AgentReleaseDefinition) -> Result<Self, AgentReleaseError> {
        definition.skills.sort_by(|a, b| a.name.cmp(&b.name));
        definition.tools.sort_by(|a, b| a.name.cmp(&b.name));
        validate_definition(&definition)?;
        let release_id = definition_id(&definition)?;
        let result = Self {
            definition,
            release_id,
        };
        if serde_json::to_vec(&result)?.len() > MAX_RELEASE_BYTES {
            return Err(AgentReleaseError::TooLarge);
        }
        Ok(result)
    }

    /// Parse a manifest from an untrusted caller, rejecting unknown fields,
    /// noncanonical ordering, unsupported schema and modified identities.
    pub fn parse_and_verify(bytes: &[u8]) -> Result<Self, AgentReleaseError> {
        if bytes.len() > MAX_RELEASE_BYTES {
            return Err(AgentReleaseError::TooLarge);
        }
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.verify()?;
        Ok(manifest)
    }

    pub fn verify(&self) -> Result<(), AgentReleaseError> {
        validate_definition(&self.definition)?;
        validate_digest(&self.release_id)?;
        if self.release_id != definition_id(&self.definition)? {
            return Err(AgentReleaseError::IdentityMismatch);
        }
        if serde_json::to_vec(self)?.len() > MAX_RELEASE_BYTES {
            return Err(AgentReleaseError::TooLarge);
        }
        Ok(())
    }
}

fn definition_id(definition: &AgentReleaseDefinition) -> Result<String, AgentReleaseError> {
    let canonical = serde_json::to_vec(definition)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(&(canonical.len() as u32).to_le_bytes());
    hasher.update(&canonical);
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn validate_definition(d: &AgentReleaseDefinition) -> Result<(), AgentReleaseError> {
    if d.schema != RELEASE_SCHEMA {
        return Err(invalid("unsupported schema"));
    }
    valid_name("agent", &d.agent)?;
    valid_name("version", &d.version)?;
    if !valid_hex64(&d.artifact_id) {
        return Err(invalid("artifact_id must be a lowercase 64-character hex identity"));
    }
    validate_digest(&d.prompt)?;
    validate_digest(&d.policy)?;
    valid_name("model provider", &d.model.provider)?;
    valid_name("model name", &d.model.model)?;
    valid_name("model revision", &d.model.revision)?;
    validate_assets("skills", &d.skills)?;
    validate_assets("tools", &d.tools)?;
    Ok(())
}

fn validate_assets(kind: &str, assets: &[NamedAsset]) -> Result<(), AgentReleaseError> {
    if assets.len() > MAX_BINDINGS {
        return Err(invalid(format!("{kind} exceeds {MAX_BINDINGS} entries")));
    }
    let mut names = HashSet::new();
    let mut previous: Option<&str> = None;
    for asset in assets {
        valid_name(kind, &asset.name)?;
        validate_digest(&asset.digest)?;
        if !names.insert(asset.name.as_str()) {
            return Err(invalid(format!("duplicate {kind} entry: {}", asset.name)));
        }
        if previous.is_some_and(|prev| prev >= asset.name.as_str()) {
            return Err(invalid(format!("{kind} must be strictly sorted by name")));
        }
        previous = Some(asset.name.as_str());
    }
    Ok(())
}

fn valid_name(field: &str, value: &str) -> Result<(), AgentReleaseError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(invalid(format!("{field} must be 1-128 ASCII letters, digits, ., _ or -")));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), AgentReleaseError> {
    if !value.starts_with("blake3:") || !valid_hex64(&value[7..]) {
        return Err(invalid("digest must be blake3:<64 lowercase hex characters>"));
    }
    Ok(())
}

fn valid_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn invalid(message: impl Into<String>) -> AgentReleaseError {
    AgentReleaseError::Invalid(message.into())
}
