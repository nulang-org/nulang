//! Compiler-owned source links for semantic effect sites.
//!
//! Nulang already gives every effect operation a backend-independent semantic
//! site identity and records the exact bytecode PC plus source-line table in
//! `CodeModule`. This module turns that existing metadata into a small,
//! versioned sidecar contract for Cloud/editor tooling. It deliberately does
//! not invent a second identity scheme and does not make source locations part
//! of semantic identity.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::bytecode::CodeModule;
use serde::{Deserialize, Serialize};
use std::fmt;

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
    pub fn from_code_module(
        _module: &CodeModule,
        _artifact: &ArtifactIdentityManifest,
        _source_path: Option<String>,
    ) -> Result<Self, ExecutionSiteMapError> {
        todo!("implemented after the contract tests")
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ExecutionSiteMapError> {
        let _ = self;
        todo!("implemented after the contract tests")
    }

    pub fn from_json(_bytes: &[u8]) -> Result<Self, ExecutionSiteMapError> {
        todo!("implemented after the contract tests")
    }
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
    InvalidSiteId(String),
    DuplicatePc(usize),
    DuplicateSiteId(String),
    InvalidLine(u32),
}

impl fmt::Display for ExecutionSiteMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
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
        let error = ExecutionSiteMap::from_code_module(
            &code_module(OpCode::Move),
            &artifact(),
            None,
        )
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
}
