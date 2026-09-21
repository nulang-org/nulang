//! Versioned RFC 0008 migration topology carried by compiled artifacts.
//!
//! Source/HIR retains executable migration bodies, while the bytecode artifact
//! boundary needs a stable representation that is independent of compiler AST
//! layout. This module records only validated migration topology and event-arm
//! metadata. Executable migration code is deliberately a separate follow-up.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ast::MigrationDecl;

pub const MIGRATION_MANIFEST_FORMAT_VERSION: u16 = 1;

/// One event arm declared by a migration contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationEventMeta {
    pub event_name: String,
    pub parameter_count: u32,
    pub catch_all: bool,
}

/// Artifact-safe description of one `migration from V to V+1` contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationContractMeta {
    pub from_version: u32,
    pub to_version: u32,
    pub has_state_transform: bool,
    /// Artifact-local hidden behavior index for the compiled state transform.
    ///
    /// Topology-only/legacy manifests may omit this. Runtime execution must
    /// require a binding before applying a state transform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_behavior_index: Option<usize>,
    pub event_transforms: Vec<MigrationEventMeta>,
}

/// Complete migration chain carried by one entity artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationManifest {
    pub format_version: u16,
    pub target_version: u32,
    pub contracts: Vec<MigrationContractMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationManifestError {
    InvalidTargetVersion(u32),
    InvalidTransition { from: u32, to: u32 },
    TransitionBeyondTarget { from: u32, to: u32, target: u32 },
    DuplicateTransition { from: u32, to: u32 },
    MissingTransition { from: u32, to: u32 },
    InvalidCurrentVersion { current: u32, target: u32 },
    InvalidJson(String),
    UnsupportedFormatVersion(u16),
}

impl fmt::Display for MigrationManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTargetVersion(version) => {
                write!(f, "entity schema version must be >= 1, got {version}")
            }
            Self::InvalidTransition { from, to } => write!(
                f,
                "invalid migration {from} -> {to}: RFC 0008 requires W == V + 1"
            ),
            Self::TransitionBeyondTarget { from, to, target } => write!(
                f,
                "migration {from} -> {to} exceeds entity target version {target}"
            ),
            Self::DuplicateTransition { from, to } => {
                write!(f, "duplicate migration transition {from} -> {to}")
            }
            Self::MissingTransition { from, to } => write!(
                f,
                "missing migration transition {from} -> {to}; RFC 0008 migration chains may not contain gaps"
            ),
            Self::InvalidCurrentVersion { current, target } => write!(
                f,
                "cannot plan migration from schema version {current} to target version {target}"
            ),
            Self::InvalidJson(error) => write!(f, "invalid migration manifest JSON: {error}"),
            Self::UnsupportedFormatVersion(version) => write!(
                f,
                "unsupported migration manifest format version {version}"
            ),
        }
    }
}

impl std::error::Error for MigrationManifestError {}

impl MigrationContractMeta {
    pub fn from_decl(decl: &MigrationDecl) -> Self {
        let mut event_transforms: Vec<_> = decl
            .event_migrations
            .iter()
            .map(|(event_name, params, _body)| MigrationEventMeta {
                event_name: event_name.clone(),
                parameter_count: params.len() as u32,
                catch_all: event_name == "other",
            })
            .collect();

        // Source order does not affect topology identity. Keep the manifest
        // canonical so equivalent contracts hash and serialize identically.
        event_transforms.sort_by(|a, b| {
            a.event_name
                .cmp(&b.event_name)
                .then(a.parameter_count.cmp(&b.parameter_count))
                .then(a.catch_all.cmp(&b.catch_all))
        });

        Self {
            from_version: decl.from_version,
            to_version: decl.to_version,
            has_state_transform: decl.state_body.is_some(),
            state_behavior_index: None,
            event_transforms,
        }
    }
}

impl MigrationManifest {
    /// Build and validate the complete RFC 0008 chain for an entity.
    pub fn from_decls(
        target_version: u32,
        migrations: &[MigrationDecl],
    ) -> Result<Self, MigrationManifestError> {
        if target_version == 0 {
            return Err(MigrationManifestError::InvalidTargetVersion(0));
        }

        let mut by_from = BTreeMap::<u32, MigrationContractMeta>::new();
        for decl in migrations {
            if decl.from_version == 0 || decl.to_version != decl.from_version.saturating_add(1) {
                return Err(MigrationManifestError::InvalidTransition {
                    from: decl.from_version,
                    to: decl.to_version,
                });
            }
            if decl.to_version > target_version {
                return Err(MigrationManifestError::TransitionBeyondTarget {
                    from: decl.from_version,
                    to: decl.to_version,
                    target: target_version,
                });
            }

            let meta = MigrationContractMeta::from_decl(decl);
            if by_from.insert(decl.from_version, meta).is_some() {
                return Err(MigrationManifestError::DuplicateTransition {
                    from: decl.from_version,
                    to: decl.to_version,
                });
            }
        }

        if target_version > 1 {
            for from in 1..target_version {
                if !by_from.contains_key(&from) {
                    return Err(MigrationManifestError::MissingTransition { from, to: from + 1 });
                }
            }
        }

        Ok(Self {
            format_version: MIGRATION_MANIFEST_FORMAT_VERSION,
            target_version,
            contracts: by_from.into_values().collect(),
        })
    }

    pub fn to_json(&self) -> Result<String, MigrationManifestError> {
        serde_json::to_string(self)
            .map_err(|error| MigrationManifestError::InvalidJson(error.to_string()))
    }

    pub fn from_json(json: &str) -> Result<Self, MigrationManifestError> {
        let manifest: Self = serde_json::from_str(json)
            .map_err(|error| MigrationManifestError::InvalidJson(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), MigrationManifestError> {
        if self.format_version != MIGRATION_MANIFEST_FORMAT_VERSION {
            return Err(MigrationManifestError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        if self.target_version == 0 {
            return Err(MigrationManifestError::InvalidTargetVersion(0));
        }

        let mut seen = BTreeSet::new();
        for contract in &self.contracts {
            if contract.from_version == 0
                || contract.to_version != contract.from_version.saturating_add(1)
            {
                return Err(MigrationManifestError::InvalidTransition {
                    from: contract.from_version,
                    to: contract.to_version,
                });
            }
            if contract.to_version > self.target_version {
                return Err(MigrationManifestError::TransitionBeyondTarget {
                    from: contract.from_version,
                    to: contract.to_version,
                    target: self.target_version,
                });
            }
            if !seen.insert(contract.from_version) {
                return Err(MigrationManifestError::DuplicateTransition {
                    from: contract.from_version,
                    to: contract.to_version,
                });
            }
        }

        if self.target_version > 1 {
            for from in 1..self.target_version {
                if !seen.contains(&from) {
                    return Err(MigrationManifestError::MissingTransition { from, to: from + 1 });
                }
            }
        }

        Ok(())
    }

    /// Return the deterministic adjacent migration chain needed to bring
    /// `current_version` to this artifact's target version.
    ///
    /// This is topology only. Callers must not treat the returned metadata as
    /// executable migration code.
    pub fn plan_from(
        &self,
        current_version: u32,
    ) -> Result<Vec<&MigrationContractMeta>, MigrationManifestError> {
        self.validate()?;
        if current_version == 0 || current_version > self.target_version {
            return Err(MigrationManifestError::InvalidCurrentVersion {
                current: current_version,
                target: self.target_version,
            });
        }
        if current_version == self.target_version {
            return Ok(Vec::new());
        }

        let by_from: BTreeMap<_, _> = self
            .contracts
            .iter()
            .map(|contract| (contract.from_version, contract))
            .collect();

        let mut plan = Vec::with_capacity((self.target_version - current_version) as usize);
        for from in current_version..self.target_version {
            let contract = by_from
                .get(&from)
                .copied()
                .ok_or(MigrationManifestError::MissingTransition {
                    from,
                    to: from + 1,
                })?;
            plan.push(contract);
        }
        Ok(plan)
    }

    /// Deterministic identity for the declarative migration topology.
    pub fn digest(&self) -> Result<String, MigrationManifestError> {
        let json = self.to_json()?;
        Ok(blake3::hash(json.as_bytes()).to_hex().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn entity_version_and_migrations(source: &str) -> (u32, Vec<MigrationDecl>) {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        ast.decls
            .into_iter()
            .find_map(|decl| match decl {
                crate::ast::Decl::Actor {
                    version,
                    migrations,
                    ..
                } => Some((version, migrations)),
                _ => None,
            })
            .expect("entity declaration")
    }

    #[test]
    fn complete_chain_round_trips_with_stable_digest() {
        let (version, migrations) = entity_version_and_migrations(
            r#"
            entity Account {
                version: 3
                state balance: Int = 0
                events
                    | Deposited(amount: Int)
                migration from 1 to 2 {
                    state => { self.balance = self.balance + 1 }
                }
                migration from 2 to 3 {
                    events {
                        | Deposited(amount) => emit Deposited(amount)
                        | other => other
                    }
                }
            }
            "#,
        );

        let manifest = MigrationManifest::from_decls(version, &migrations).unwrap();
        assert_eq!(manifest.contracts.len(), 2);
        assert!(manifest.contracts[0].has_state_transform);
        assert_eq!(manifest.contracts[1].event_transforms.len(), 2);

        let json = manifest.to_json().unwrap();
        let restored = MigrationManifest::from_json(&json).unwrap();
        assert_eq!(restored, manifest);
        assert_eq!(restored.digest().unwrap(), manifest.digest().unwrap());
    }

    #[test]
    fn plan_from_intermediate_version_is_adjacent_and_ordered() {
        let (version, migrations) = entity_version_and_migrations(
            r#"
            entity Account {
                version: 4
                state balance: Int = 0
                migration from 1 to 2 { state => { self.balance = self.balance + 1 } }
                migration from 2 to 3 { state => { self.balance = self.balance + 1 } }
                migration from 3 to 4 { state => { self.balance = self.balance + 1 } }
            }
            "#,
        );
        let manifest = MigrationManifest::from_decls(version, &migrations).unwrap();
        let plan = manifest.plan_from(2).unwrap();
        assert_eq!(
            plan.iter()
                .map(|step| (step.from_version, step.to_version))
                .collect::<Vec<_>>(),
            vec![(2, 3), (3, 4)]
        );
    }

    #[test]
    fn rejects_gap_in_chain() {
        let (version, migrations) = entity_version_and_migrations(
            r#"
            entity Account {
                version: 3
                state balance: Int = 0
                migration from 2 to 3 {
                    state => { self.balance = self.balance + 1 }
                }
            }
            "#,
        );
        assert_eq!(
            MigrationManifest::from_decls(version, &migrations),
            Err(MigrationManifestError::MissingTransition { from: 1, to: 2 })
        );
    }

    #[test]
    fn rejects_non_adjacent_transition() {
        let (version, migrations) = entity_version_and_migrations(
            r#"
            entity Account {
                version: 3
                state balance: Int = 0
                migration from 1 to 3 {
                    state => { self.balance = self.balance + 1 }
                }
            }
            "#,
        );
        assert_eq!(
            MigrationManifest::from_decls(version, &migrations),
            Err(MigrationManifestError::InvalidTransition { from: 1, to: 3 })
        );
    }

    #[test]
    fn version_one_requires_no_manifest_edges() {
        let manifest = MigrationManifest::from_decls(1, &[]).unwrap();
        assert!(manifest.contracts.is_empty());
        manifest.validate().unwrap();
        assert!(manifest.plan_from(1).unwrap().is_empty());
    }
}
