//! Durable-branch lifecycle metadata.
//!
//! This module complements `dap::rewind`: rewind/branch capture reconstructs
//! historical state, while `BranchManifest` pins the execution identity that a
//! future activated branch must use. Keeping the manifest separate from the
//! debugger representation lets tooling inspect historical branches without
//! accidentally making them executable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::dap::rewind::DurableBranch;

/// Current serialized manifest schema version.
pub const BRANCH_MANIFEST_VERSION: u16 = 1;

/// Content/version identity required before a durable branch can be considered
/// for activation or shadow replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchManifest {
    pub manifest_version: u16,
    pub branch_id: String,
    pub parent_entity_id: u64,
    pub fork_sequence: u64,
    /// BLAKE3 content identity of the compiled Nulang artifact (`.nbc`).
    pub code_hash: String,
    pub schema_version: u32,
    pub runtime_version: String,
    pub journal_format_version: u32,
    /// Hybrid/logical timestamp serialized by the caller. The runtime does not
    /// assume a wall-clock representation here.
    pub created_at_hlc: String,
    pub created_by_principal: String,
    /// Explicit capability restrictions inherited or attenuated for the branch.
    #[serde(default)]
    pub capability_envelope: BTreeMap<String, String>,
    /// Optional hash of the signed execution-provenance record that created the
    /// branch. Nulang Cloud can bind this to its audit/provenance subsystem.
    pub provenance_hash: Option<String>,
}

impl BranchManifest {
    /// Construct a version-pinned manifest from an immutable historical branch.
    ///
    /// This does not activate the branch. It only captures the metadata needed
    /// to make later replay/activation reproducible.
    #[allow(clippy::too_many_arguments)]
    pub fn from_branch(
        branch: &DurableBranch,
        code_hash: impl Into<String>,
        schema_version: u32,
        runtime_version: impl Into<String>,
        journal_format_version: u32,
        created_at_hlc: impl Into<String>,
        created_by_principal: impl Into<String>,
    ) -> Self {
        Self {
            manifest_version: BRANCH_MANIFEST_VERSION,
            branch_id: branch.branch_id.clone(),
            parent_entity_id: branch.parent_actor_id,
            fork_sequence: branch.fork_sequence,
            code_hash: code_hash.into(),
            schema_version,
            runtime_version: runtime_version.into(),
            journal_format_version,
            created_at_hlc: created_at_hlc.into(),
            created_by_principal: created_by_principal.into(),
            capability_envelope: BTreeMap::new(),
            provenance_hash: None,
        }
    }

    pub fn with_capability(
        mut self,
        capability: impl Into<String>,
        constraint: impl Into<String>,
    ) -> Self {
        self.capability_envelope
            .insert(capability.into(), constraint.into());
        self
    }

    pub fn with_provenance_hash(mut self, hash: impl Into<String>) -> Self {
        self.provenance_hash = Some(hash.into());
        self
    }

    /// Validate invariants that must hold before shadow replay or activation.
    ///
    /// Validation is intentionally strict: silently falling back to the
    /// currently-deployed artifact/runtime would make historical branches
    /// non-reproducible.
    pub fn validate(&self) -> Result<(), BranchManifestError> {
        if self.manifest_version != BRANCH_MANIFEST_VERSION {
            return Err(BranchManifestError::UnsupportedManifestVersion(
                self.manifest_version,
            ));
        }
        if self.branch_id.trim().is_empty() {
            return Err(BranchManifestError::Missing("branch_id"));
        }
        if self.code_hash.trim().is_empty() {
            return Err(BranchManifestError::Missing("code_hash"));
        }
        if self.runtime_version.trim().is_empty() {
            return Err(BranchManifestError::Missing("runtime_version"));
        }
        if self.created_at_hlc.trim().is_empty() {
            return Err(BranchManifestError::Missing("created_at_hlc"));
        }
        if self.created_by_principal.trim().is_empty() {
            return Err(BranchManifestError::Missing("created_by_principal"));
        }
        Ok(())
    }

    /// Canonical bytes used for content identity and provenance binding.
    ///
    /// `BTreeMap` keeps capability keys deterministically ordered.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, BranchManifestError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|e| BranchManifestError::Serialization(e.to_string()))
    }

    /// BLAKE3 content identity for the manifest itself.
    pub fn digest(&self) -> Result<String, BranchManifestError> {
        Ok(blake3::hash(&self.canonical_bytes()?).to_hex().to_string())
    }

    /// Check whether a runtime/artifact tuple exactly matches this manifest.
    /// Activation code should perform this check before loading branch state.
    pub fn matches_execution_identity(
        &self,
        code_hash: &str,
        schema_version: u32,
        runtime_version: &str,
        journal_format_version: u32,
    ) -> bool {
        self.code_hash == code_hash
            && self.schema_version == schema_version
            && self.runtime_version == runtime_version
            && self.journal_format_version == journal_format_version
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchManifestError {
    UnsupportedManifestVersion(u16),
    Missing(&'static str),
    Serialization(String),
}

impl std::fmt::Display for BranchManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedManifestVersion(v) => {
                write!(f, "unsupported branch manifest version {v}")
            }
            Self::Missing(field) => {
                write!(f, "branch manifest is missing required field '{field}'")
            }
            Self::Serialization(msg) => write!(f, "branch manifest serialization failed: {msg}"),
        }
    }
}

impl std::error::Error for BranchManifestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{EventEntry, JournalEntry, PersistedValue};

    fn branch() -> DurableBranch {
        DurableBranch {
            branch_id: "branch-1".into(),
            parent_actor_id: 42,
            fork_sequence: 9,
            parent_latest_sequence: 12,
            snapshot_sequence: 8,
            state: BTreeMap::from([("count".into(), PersistedValue::Int(7))]),
            journal: vec![JournalEntry {
                sequence: 9,
                behavior_id: 1,
                payload: vec![],
            }],
            events: vec![EventEntry {
                sequence: 9,
                field_name: "count".into(),
                event_name: "Changed".into(),
                args: vec![],
                value: PersistedValue::Int(7),
            }],
        }
    }

    fn manifest() -> BranchManifest {
        BranchManifest::from_branch(
            &branch(),
            "blake3:abc123",
            4,
            "nulang-runtime/0.1.0",
            1,
            "1700000000:4:node-a",
            "principal:user-7",
        )
    }

    #[test]
    fn manifest_captures_branch_lineage_and_versions() {
        let m = manifest();
        assert_eq!(m.branch_id, "branch-1");
        assert_eq!(m.parent_entity_id, 42);
        assert_eq!(m.fork_sequence, 9);
        assert_eq!(m.schema_version, 4);
        assert_eq!(m.journal_format_version, 1);
        assert!(m.validate().is_ok());
    }

    #[test]
    fn manifest_digest_is_deterministic() {
        let a = manifest()
            .with_capability("Payments.capture", "amount<=500")
            .with_capability("Net.fetch", "host=example.com");
        let b = manifest()
            .with_capability("Net.fetch", "host=example.com")
            .with_capability("Payments.capture", "amount<=500");
        assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    }

    #[test]
    fn execution_identity_must_match_exactly() {
        let m = manifest();
        assert!(m.matches_execution_identity("blake3:abc123", 4, "nulang-runtime/0.1.0", 1));
        assert!(!m.matches_execution_identity("blake3:different", 4, "nulang-runtime/0.1.0", 1));
        assert!(!m.matches_execution_identity("blake3:abc123", 5, "nulang-runtime/0.1.0", 1));
    }

    #[test]
    fn manifest_rejects_missing_code_identity() {
        let mut m = manifest();
        m.code_hash.clear();
        assert_eq!(m.validate(), Err(BranchManifestError::Missing("code_hash")));
    }

    #[test]
    fn manifest_round_trips_json() {
        let m = manifest().with_provenance_hash("prov:deadbeef");
        let json = serde_json::to_string(&m).unwrap();
        let decoded: BranchManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, m);
    }
}
