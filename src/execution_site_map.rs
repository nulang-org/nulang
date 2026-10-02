//! Compiler-owned source links for semantic effect sites.
//!
//! Nulang already gives every effect operation a backend-independent semantic
//! site identity, and MIR retains source-line metadata independently of the
//! eventual bytecode/WASM/native backend. This module turns those compiler-
//! owned facts into a small versioned sidecar contract for Cloud/editor tooling.
//! Source locations are presentation metadata only and never participate in
//! semantic-site identity.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::content_identity::{ArtifactId, SemanticId, SourceId};
use crate::mir::{self, BlockId, RValue, Stmt};
use crate::semantic_identity::{effect_sites_for_mir, EffectSiteOwnerKind as SemanticOwnerKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};
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
    /// Compiler-owned backend-independent semantic effect-site digest.
    pub semantic_site_id: String,
    pub owner_kind: ExecutionSiteOwnerKind,
    pub owner_name: String,
    pub effect_operation: String,
    /// Zero-based occurrence among the same qualified operation in this owner.
    pub operation_ordinal: u32,
    /// One-indexed source line when MIR source metadata is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionSiteOwnerKind {
    Function,
    Behavior,
}

impl ExecutionSiteMap {
    /// Build a portable source-link map directly from backend-neutral MIR.
    ///
    /// Semantic identities come exclusively from `effect_sites_for_mir`; this
    /// module performs a parallel source-location walk only to attach line
    /// metadata. Any disagreement between the two traversals fails closed.
    pub fn from_mir_module(
        module: &mir::Module,
        artifact: &ArtifactIdentityManifest,
        source_path: Option<String>,
    ) -> Result<Self, ExecutionSiteMapError> {
        if module.name.trim().is_empty() {
            return Err(ExecutionSiteMapError::InvalidModule(
                "module name must not be empty".to_string(),
            ));
        }
        if let Some(path) = source_path.as_deref() {
            validate_source_path(path)?;
        }

        let semantic_sites = effect_sites_for_mir(module);
        let source_sites = source_sites_for_mir(module);
        if semantic_sites.len() != source_sites.len() {
            return Err(ExecutionSiteMapError::SiteMetadataCountMismatch {
                semantic_sites: semantic_sites.len(),
                source_sites: source_sites.len(),
            });
        }

        let mut sites = Vec::with_capacity(semantic_sites.len());
        for (index, (semantic, source)) in semantic_sites
            .into_iter()
            .zip(source_sites.into_iter())
            .enumerate()
        {
            let owner_kind = owner_kind(semantic.owner_kind);
            if owner_kind != source.owner_kind
                || semantic.owner_name != source.owner_name
                || semantic.effect_operation != source.effect_operation
            {
                return Err(ExecutionSiteMapError::SiteMetadataMismatch { index });
            }

            sites.push(ExecutionSite {
                semantic_site_id: semantic.id.to_hex(),
                owner_kind,
                owner_name: semantic.owner_name,
                effect_operation: semantic.effect_operation,
                operation_ordinal: semantic.operation_ordinal,
                line: source.line,
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

    /// Serialize deterministic human-readable JSON for sidecar/tooling use.
    pub fn to_json(&self) -> Result<Vec<u8>, ExecutionSiteMapError> {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.validate()?;
        serde_json::to_vec_pretty(&normalized).map_err(ExecutionSiteMapError::from)
    }

    /// Parse untrusted site-map JSON and fail closed on malformed identities,
    /// duplicate sites, unsafe source paths, invalid source locations, or an
    /// unsupported schema.
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
        self.sites
            .sort_by(|left, right| left.semantic_site_id.cmp(&right.semantic_site_id));
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
        if let Some(path) = self.source_path.as_deref() {
            validate_source_path(path)?;
        }

        let mut ids = BTreeSet::new();
        for site in &self.sites {
            validate_site_id(&site.semantic_site_id)?;
            if !ids.insert(site.semantic_site_id.clone()) {
                return Err(ExecutionSiteMapError::DuplicateSiteId(
                    site.semantic_site_id.clone(),
                ));
            }
            if site.owner_name.trim().is_empty() {
                return Err(ExecutionSiteMapError::InvalidOwnerName(
                    site.semantic_site_id.clone(),
                ));
            }
            if site.effect_operation.trim().is_empty() {
                return Err(ExecutionSiteMapError::InvalidEffectOperation(
                    site.semantic_site_id.clone(),
                ));
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceSite {
    owner_kind: ExecutionSiteOwnerKind,
    owner_name: String,
    effect_operation: String,
    line: Option<u32>,
}

fn source_sites_for_mir(module: &mir::Module) -> Vec<SourceSite> {
    let mut sites = Vec::new();
    for function in &module.functions {
        collect_source_sites(
            &mut sites,
            ExecutionSiteOwnerKind::Function,
            function,
        );
    }
    for behavior in &module.behaviors {
        collect_source_sites(
            &mut sites,
            ExecutionSiteOwnerKind::Behavior,
            behavior,
        );
    }
    sites
}

fn collect_source_sites(
    out: &mut Vec<SourceSite>,
    owner_kind: ExecutionSiteOwnerKind,
    function: &mir::Function,
) {
    let line_table: BTreeMap<(BlockId, usize), u32> =
        function.line_table.iter().copied().collect();

    for block in &function.blocks {
        for (stmt_index, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Assign { op, .. } = stmt else {
                continue;
            };
            let effect_operation = match op {
                RValue::Perform { effect, op, .. } => format!("{effect}.{op}"),
                RValue::PerformAsync { effect_op, .. } => effect_op.clone(),
                _ => continue,
            };
            out.push(SourceSite {
                owner_kind,
                owner_name: function.name.clone(),
                effect_operation,
                line: line_table.get(&(block.id, stmt_index)).copied(),
            });
        }
    }
}

fn owner_kind(kind: SemanticOwnerKind) -> ExecutionSiteOwnerKind {
    match kind {
        SemanticOwnerKind::Function => ExecutionSiteOwnerKind::Function,
        SemanticOwnerKind::Behavior => ExecutionSiteOwnerKind::Behavior,
    }
}

fn validate_source_path(value: &str) -> Result<(), ExecutionSiteMapError> {
    if value.trim().is_empty() {
        return Err(ExecutionSiteMapError::InvalidSourcePath(value.to_string()));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ExecutionSiteMapError::InvalidSourcePath(value.to_string()));
    }
    Ok(())
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
    InvalidSourcePath(String),
    SiteMetadataCountMismatch {
        semantic_sites: usize,
        source_sites: usize,
    },
    SiteMetadataMismatch {
        index: usize,
    },
    InvalidOwnerName(String),
    InvalidEffectOperation(String),
    InvalidSiteId(String),
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
            Self::InvalidSourcePath(path) => {
                write!(f, "source path must be safe and package-relative: {path}")
            }
            Self::SiteMetadataCountMismatch {
                semantic_sites,
                source_sites,
            } => write!(
                f,
                "semantic/source effect-site counts differ: {semantic_sites} vs {source_sites}"
            ),
            Self::SiteMetadataMismatch { index } => {
                write!(f, "semantic/source effect-site metadata differs at index {index}")
            }
            Self::InvalidOwnerName(id) => write!(f, "execution site {id} has an empty owner name"),
            Self::InvalidEffectOperation(id) => {
                write!(f, "execution site {id} has an empty effect operation")
            }
            Self::InvalidSiteId(id) => write!(f, "invalid semantic execution-site id {id}"),
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
    use crate::content_identity::{SemanticId, SourceId};
    use crate::mir::{FunctionBuilder, Module, RValue, Terminator};
    use crate::semantic_identity::effect_sites_for_mir;
    use crate::types::Type;

    fn artifact() -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(b"perform Payments.charge(amount)")),
            SemanticId::from_canonical_bytes(b"orders-semantic", []),
            "nulang-test",
            "wasm32-wasip2",
            "abi-v1",
            "wasm",
            std::iter::empty::<&str>(),
        )
    }

    fn mir_module(line: u32) -> Module {
        let mut function = FunctionBuilder::new("checkout", None);
        let dst = function.add_temp(Type::int());
        function.set_line(line);
        function.assign(
            dst,
            RValue::PerformAsync {
                effect_op: "Payments.charge".to_string(),
                args: Vec::new(),
                resolved_handler: None,
            },
        );
        function.terminate(Terminator::Return(None));

        let mut module = Module::new("orders");
        module.functions.push(function.build());
        module
    }

    #[test]
    fn maps_backend_independent_semantic_site_to_source_line() {
        let module = mir_module(12);
        let expected_id = effect_sites_for_mir(&module)[0].id.to_hex();
        let map = ExecutionSiteMap::from_mir_module(
            &module,
            &artifact(),
            Some("src/orders.nula".to_string()),
        )
        .expect("site map should build");

        assert_eq!(map.schema, EXECUTION_SITE_MAP_SCHEMA);
        assert_eq!(map.module, "orders");
        assert_eq!(map.source_path.as_deref(), Some("src/orders.nula"));
        assert_eq!(map.sites.len(), 1);
        assert_eq!(map.sites[0].semantic_site_id, expected_id);
        assert_eq!(map.sites[0].owner_kind, ExecutionSiteOwnerKind::Function);
        assert_eq!(map.sites[0].owner_name, "checkout");
        assert_eq!(map.sites[0].effect_operation, "Payments.charge");
        assert_eq!(map.sites[0].operation_ordinal, 0);
        assert_eq!(map.sites[0].line, Some(12));
    }

    #[test]
    fn source_line_changes_do_not_change_semantic_site_identity() {
        let first = ExecutionSiteMap::from_mir_module(&mir_module(12), &artifact(), None).unwrap();
        let second = ExecutionSiteMap::from_mir_module(&mir_module(99), &artifact(), None).unwrap();

        assert_eq!(first.sites[0].semantic_site_id, second.sites[0].semantic_site_id);
        assert_eq!(first.sites[0].line, Some(12));
        assert_eq!(second.sites[0].line, Some(99));
    }

    #[test]
    fn module_without_effect_sites_produces_empty_map() {
        let module = Module::new("legacy");
        let map = ExecutionSiteMap::from_mir_module(&module, &artifact(), None)
            .expect("no effect sites is a valid map");
        assert!(map.sites.is_empty());
    }

    #[test]
    fn rejects_parent_traversal_source_path() {
        let error = ExecutionSiteMap::from_mir_module(
            &mir_module(12),
            &artifact(),
            Some("../orders.nula".to_string()),
        )
        .expect_err("source paths must be package-relative");

        assert!(matches!(error, ExecutionSiteMapError::InvalidSourcePath(_)));
    }

    #[test]
    fn json_roundtrip_is_deterministic() {
        let map = ExecutionSiteMap::from_mir_module(
            &mir_module(12),
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
        let mut map =
            ExecutionSiteMap::from_mir_module(&mir_module(12), &artifact(), None).unwrap();
        let mut duplicate = map.sites[0].clone();
        duplicate.owner_name = "other".to_string();
        map.sites.push(duplicate);
        let bytes = serde_json::to_vec(&map).unwrap();

        assert!(matches!(
            ExecutionSiteMap::from_json(&bytes),
            Err(ExecutionSiteMapError::DuplicateSiteId(_))
        ));
    }

    #[test]
    fn parser_normalizes_uppercase_site_ids() {
        let map = ExecutionSiteMap::from_mir_module(&mir_module(12), &artifact(), None).unwrap();
        let mut value = serde_json::to_value(map).unwrap();
        let id = value["sites"][0]["semantic_site_id"]
            .as_str()
            .unwrap()
            .to_ascii_uppercase();
        value["sites"][0]["semantic_site_id"] = serde_json::Value::String(id);

        let parsed = ExecutionSiteMap::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(parsed.sites[0]
            .semantic_site_id
            .bytes()
            .all(|byte| !byte.is_ascii_uppercase()));
    }

    #[test]
    fn parser_rejects_malformed_site_id() {
        let map = ExecutionSiteMap::from_mir_module(&mir_module(12), &artifact(), None).unwrap();
        let mut value = serde_json::to_value(map).unwrap();
        value["sites"][0]["semantic_site_id"] =
            serde_json::Value::String("not-a-site-id".to_string());

        assert!(matches!(
            ExecutionSiteMap::from_json(&serde_json::to_vec(&value).unwrap()),
            Err(ExecutionSiteMapError::InvalidSiteId(_))
        ));
    }
}
