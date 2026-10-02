//! Compiler-owned source links for semantic effect sites.
//!
//! Nulang already gives every effect operation a backend-independent semantic
//! site identity and records the exact bytecode PC plus source-line table in
//! `CodeModule`. This module turns that existing metadata into a small,
//! versioned sidecar contract for Cloud/editor tooling. It deliberately does
//! not invent a second identity scheme and does not make source locations part
//! of semantic identity.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::bytecode::{CodeModule, OpCode};
use crate::content_identity::{ArtifactId, SemanticId, SourceId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

pub const EXECUTION_SITE_MAP_SCHEMA: &str = "nulang.execution-sites/v0alpha1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSiteMap {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub semantic_id: String,
    pub artifact_id: String,
    pub module: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<ExecutionSite>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSite {
    /// Artifact-local bytecode program counter. This is lookup metadata only;
    /// it does not participate in semantic-site identity.
    pub pc: usize,
    /// Compiler-owned semantic effect-site digest, encoded as 64 lowercase
    /// hexadecimal characters.
    pub semantic_site_id: String,
    pub effect_operation: String,
    /// One-indexed source line when debug/source metadata is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl ExecutionSiteMap {
    /// Build a source-link map from compiler-owned artifact metadata.
    ///
    /// The semantic site ID is copied from `CodeModule::effect_sites`; source
    /// lines are presentation metadata looked up through `CodeModule::line_at`.
    /// The constructor fails closed if an effect-site entry points outside the
    /// artifact or at a non-effect opcode.
    pub fn from_code_module(
        module: &CodeModule,
        artifact: &ArtifactIdentityManifest,
        source_path: Option<String>,
    ) -> Result<Self, ExecutionSiteMapError> {
        if module.name.trim().is_empty() {
            return Err(ExecutionSiteMapError::InvalidModule(
                "module name must not be empty".to_string(),
            ));
        }
        if source_path
            .as_deref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(ExecutionSiteMapError::InvalidSourcePath);
        }

        let mut sites = Vec::with_capacity(module.effect_sites.len());
        for site in &module.effect_sites {
            let instruction =
                module
                    .instructions
                    .get(site.pc)
                    .ok_or(ExecutionSiteMapError::InvalidSitePc {
                        pc: site.pc,
                        instruction_count: module.instructions.len(),
                    })?;
            if !matches!(
                instruction.opcode,
                OpCode::Perform | OpCode::PerformDirect | OpCode::PerformAsync
            ) {
                return Err(ExecutionSiteMapError::InvalidEffectOpcode { pc: site.pc });
            }

            sites.push(ExecutionSite {
                pc: site.pc,
                semantic_site_id: hex::encode(site.id),
                effect_operation: site.effect_operation.clone(),
                line: module.line_at(site.pc),
            });
        }

        let mut map = Self {
            schema: EXECUTION_SITE_MAP_SCHEMA.to_string(),
            source_id: artifact.source_id().map(|id| id.to_string()),
            semantic_id: artifact.semantic_id().to_string(),
            artifact_id: artifact.artifact_id().to_string(),
            module: module.name.clone(),
            source_path,
            sites,
        };
        map.normalize();
        map.validate()?;
        Ok(map)
    }

    /// Serialize canonical human-readable JSON for sidecar/tooling use.
    pub fn to_json(&self) -> Result<Vec<u8>, ExecutionSiteMapError> {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.validate()?;
        serde_json::to_vec_pretty(&normalized).map_err(ExecutionSiteMapError::from)
    }

    /// Parse untrusted site-map JSON and fail closed on malformed identities,
    /// duplicate sites, invalid source locations, or an unsupported schema.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ExecutionSiteMapError> {
        let mut map: Self = serde_json::from_slice(bytes).map_err(ExecutionSiteMapError::from)?;
        map.normalize();
        map.validate()?;
        Ok(map)
    }

    fn normalize(&mut self) {
        for site in &mut self.sites {
            site.semantic_site_id.make_ascii_lowercase();
        }
        self.sites.sort_by_key(|site| site.pc);
    }

    fn validate(&self) -> Result<(), ExecutionSiteMapError> {
        if self.schema != EXECUTION_SITE_MAP_SCHEMA {
            return Err(ExecutionSiteMapError::UnsupportedSchema(
                self.schema.clone(),
            ));
        }
        if let Some(source_id) = &self.source_id {
            parse_identity::<SourceId>("source_id", source_id)?;
        }
        parse_identity::<SemanticId>("semantic_id", &self.semantic_id)?;
        parse_identity::<ArtifactId>("artifact_id", &self.artifact_id)?;

        if self.module.trim().is_empty() {
            return Err(ExecutionSiteMapError::InvalidModule(
                "module name must not be empty".to_string(),
            ));
        }
        if self
            .source_path
            .as_deref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(ExecutionSiteMapError::InvalidSourcePath);
        }

        let mut pcs = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for site in &self.sites {
            if !pcs.insert(site.pc) {
                return Err(ExecutionSiteMapError::DuplicatePc(site.pc));
            }
            validate_site_id(&site.semantic_site_id)?;
            if !ids.insert(site.semantic_site_id.clone()) {
                return Err(ExecutionSiteMapError::DuplicateSiteId(
                    site.semantic_site_id.clone(),
                ));
            }
            if site.effect_operation.trim().is_empty() {
                return Err(ExecutionSiteMapError::InvalidEffectOperation { pc: site.pc });
            }
            if let Some(line) = site.line {
                if line == 0 {
                    return Err(ExecutionSiteMapError::InvalidLine(line));
                }
            }
        }

        Ok(())
    }
}

fn parse_identity<T>(field: &'static str, value: &str) -> Result<T, ExecutionSiteMapError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| ExecutionSiteMapError::InvalidIdentity {
            field,
            message: error.to_string(),
        })
}

fn validate_site_id(value: &str) -> Result<(), ExecutionSiteMapError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExecutionSiteMapError::InvalidSiteId(value.to_string()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionSiteMapError {
    Json(String),
    UnsupportedSchema(String),
    InvalidIdentity {
        field: &'static str,
        message: String,
    },
    InvalidModule(String),
    InvalidSourcePath,
    InvalidSitePc {
        pc: usize,
        instruction_count: usize,
    },
    InvalidEffectOpcode {
        pc: usize,
    },
    InvalidEffectOperation {
        pc: usize,
    },
    InvalidSiteId(String),
    DuplicatePc(usize),
    DuplicateSiteId(String),
    InvalidLine(u32),
}

impl fmt::Display for ExecutionSiteMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid execution-site map JSON: {message}"),
            Self::UnsupportedSchema(schema) => {
                write!(f, "unsupported execution-site map schema {schema}")
            }
            Self::InvalidIdentity { field, message } => {
                write!(f, "invalid {field} in execution-site map: {message}")
            }
            Self::InvalidModule(message) => write!(f, "invalid module: {message}"),
            Self::InvalidSourcePath => write!(f, "source path must not be empty"),
            Self::InvalidSitePc {
                pc,
                instruction_count,
            } => write!(
                f,
                "execution site pc {pc} is outside artifact instruction count {instruction_count}"
            ),
            Self::InvalidEffectOpcode { pc } => {
                write!(
                    f,
                    "execution site pc {pc} does not point at an effect opcode"
                )
            }
            Self::InvalidEffectOperation { pc } => {
                write!(f, "execution site pc {pc} has an empty effect operation")
            }
            Self::InvalidSiteId(id) => write!(f, "invalid semantic execution-site id {id}"),
            Self::DuplicatePc(pc) => write!(f, "duplicate execution site pc {pc}"),
            Self::DuplicateSiteId(id) => write!(f, "duplicate semantic execution-site id {id}"),
            Self::InvalidLine(line) => write!(f, "source lines are one-indexed; got {line}"),
        }
    }
}

impl std::error::Error for ExecutionSiteMapError {}

impl From<serde_json::Error> for ExecutionSiteMapError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_identity::ArtifactIdentityManifest;
    use crate::bytecode::{EffectSiteMetadata, Instruction, OpCode};
    use crate::content_identity::{SemanticId, SourceId};

    fn artifact() -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(b"perform Payments.charge(amount)")),
            SemanticId::from_canonical_bytes(b"orders-semantic", []),
            "nulang-test",
            "test-target",
            "abi-v1",
            "bytecode",
            std::iter::empty::<&str>(),
        )
    }

    fn code_module(opcode: OpCode) -> CodeModule {
        let mut module = CodeModule::new("orders");
        module.instructions = vec![
            Instruction::new0(OpCode::Move),
            Instruction::new0(OpCode::Move),
            Instruction::new0(opcode),
        ];
        module.line_table = vec![(0, 10), (2, 12)];
        module.effect_sites = vec![EffectSiteMetadata {
            pc: 2,
            id: [0xab; 32],
            effect_operation: "Payments.charge".to_string(),
        }];
        module
    }

    #[test]
    fn maps_existing_semantic_effect_site_to_source_line() {
        let map = ExecutionSiteMap::from_code_module(
            &code_module(OpCode::PerformAsync),
            &artifact(),
            Some("src/orders.nula".to_string()),
        )
        .expect("site map should build");

        assert_eq!(map.schema, EXECUTION_SITE_MAP_SCHEMA);
        assert_eq!(map.module, "orders");
        assert_eq!(map.source_path.as_deref(), Some("src/orders.nula"));
        assert_eq!(map.sites.len(), 1);
        assert_eq!(map.sites[0].pc, 2);
        assert_eq!(map.sites[0].effect_operation, "Payments.charge");
        assert_eq!(map.sites[0].line, Some(12));
        assert_eq!(map.sites[0].semantic_site_id, "ab".repeat(32));
    }

    #[test]
    fn rejects_metadata_that_points_at_a_non_effect_opcode() {
        let error =
            ExecutionSiteMap::from_code_module(&code_module(OpCode::Move), &artifact(), None)
                .expect_err("invalid site metadata must fail closed");

        assert_eq!(error, ExecutionSiteMapError::InvalidEffectOpcode { pc: 2 });
    }

    #[test]
    fn legacy_artifact_without_effect_sites_produces_empty_map() {
        let mut module = CodeModule::new("legacy");
        module.instructions.push(Instruction::new0(OpCode::Ret));

        let map = ExecutionSiteMap::from_code_module(&module, &artifact(), None)
            .expect("missing additive metadata is backward compatible");
        assert!(map.sites.is_empty());
    }

    #[test]
    fn json_roundtrip_is_deterministic() {
        let map = ExecutionSiteMap::from_code_module(
            &code_module(OpCode::PerformAsync),
            &artifact(),
            Some("src/orders.nula".to_string()),
        )
        .unwrap();

        let first = map.to_json().unwrap();
        let parsed = ExecutionSiteMap::from_json(&first).unwrap();
        let second = parsed.to_json().unwrap();

        assert_eq!(parsed, map);
        assert_eq!(first, second);
    }

    #[test]
    fn json_parser_rejects_duplicate_semantic_site_ids() {
        let mut map = ExecutionSiteMap::from_code_module(
            &code_module(OpCode::PerformAsync),
            &artifact(),
            None,
        )
        .unwrap();
        let mut duplicate = map.sites[0].clone();
        duplicate.pc = 1;
        map.sites.push(duplicate);
        let bytes = serde_json::to_vec(&map).unwrap();

        assert!(matches!(
            ExecutionSiteMap::from_json(&bytes),
            Err(ExecutionSiteMapError::DuplicateSiteId(_))
        ));
    }

    #[test]
    fn parser_normalizes_uppercase_site_ids() {
        let map = ExecutionSiteMap::from_code_module(
            &code_module(OpCode::PerformAsync),
            &artifact(),
            None,
        )
        .unwrap();
        let mut value = serde_json::to_value(map).unwrap();
        value["sites"][0]["semantic_site_id"] = serde_json::Value::String("AB".repeat(32));

        let parsed = ExecutionSiteMap::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.sites[0].semantic_site_id, "ab".repeat(32));
    }
}
