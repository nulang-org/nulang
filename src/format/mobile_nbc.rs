//! Additive mobile metadata for `.nbc` artifacts.
//!
//! The canonical `.nbc` decoder intentionally ignores bytes after the base
//! metadata body. Mobile artifacts use that compatibility property to append a
//! small, versioned trailer rather than changing `CodeModule` or the frozen v1
//! bytecode layout. Old runtimes continue to load the bytecode and ignore the
//! trailer; mobile-aware runtimes validate and retain the authorization table.

use crate::bytecode::CodeModule;
use crate::format::constants::FormatError;
use crate::format::nbc::NbcArtifact;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// Magic prefix for the optional mobile-action metadata trailer.
pub const MOBILE_ACTION_MAGIC: [u8; 4] = *b"NLMA";
/// Current mobile-action trailer version.
pub const MOBILE_ACTION_VERSION: u32 = 1;
/// Frozen client reducer ABI required by compiler-authorized mobile actions.
pub const CLIENT_ACTION_ABI: &str = "nulang-action-reducer/1";

/// One compiler-authorized client action.
///
/// `function_index` indexes `CodeModule::function_table`; the handler name is
/// retained for audit/debugging, while the index is the execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientActionEntry {
    pub action_id: String,
    pub handler: String,
    pub function_index: u32,
}

/// Host-facing mobile action authorization metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileActionMetadata {
    pub abi: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_actions: Vec<ClientActionEntry>,
}

impl Default for MobileActionMetadata {
    fn default() -> Self {
        Self {
            abi: CLIENT_ACTION_ABI.to_owned(),
            client_actions: Vec::new(),
        }
    }
}

impl MobileActionMetadata {
    /// Validate metadata against the decoded bytecode module.
    pub fn validate(&self, module: &CodeModule) -> Result<(), MobileNbcError> {
        if self.abi != CLIENT_ACTION_ABI {
            return Err(MobileNbcError::UnsupportedAbi(self.abi.clone()));
        }

        let mut ids = HashSet::with_capacity(self.client_actions.len());
        for action in &self.client_actions {
            if action.action_id.trim().is_empty() {
                return Err(MobileNbcError::InvalidAction(
                    "action_id must not be empty".to_owned(),
                ));
            }
            if action.handler.trim().is_empty() {
                return Err(MobileNbcError::InvalidAction(format!(
                    "action '{}' has an empty handler name",
                    action.action_id
                )));
            }
            if !ids.insert(action.action_id.as_str()) {
                return Err(MobileNbcError::InvalidAction(format!(
                    "duplicate action_id '{}'",
                    action.action_id
                )));
            }
            let index = action.function_index as usize;
            if index >= module.function_table.len() {
                return Err(MobileNbcError::InvalidAction(format!(
                    "action '{}' references function index {} but module has {} functions",
                    action.action_id,
                    action.function_index,
                    module.function_table.len()
                )));
            }
        }
        Ok(())
    }
}

/// A decoded `.nbc` artifact plus optional mobile action authorization.
#[derive(Debug, Clone)]
pub struct MobileNbcArtifact {
    pub artifact: NbcArtifact,
    pub mobile_actions: MobileActionMetadata,
}

#[derive(Debug)]
pub enum MobileNbcError {
    Base(FormatError),
    TruncatedTrailer,
    BadTrailerMagic([u8; 4]),
    UnsupportedTrailerVersion(u32),
    TrailerLengthMismatch { declared: usize, actual: usize },
    Decode(String),
    UnsupportedAbi(String),
    InvalidAction(String),
}

impl fmt::Display for MobileNbcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base(err) => write!(f, "invalid .nbc artifact: {err}"),
            Self::TruncatedTrailer => write!(f, "truncated mobile action metadata trailer"),
            Self::BadTrailerMagic(got) => write!(
                f,
                "invalid mobile action metadata magic: expected {:?}, got {:?}",
                MOBILE_ACTION_MAGIC, got
            ),
            Self::UnsupportedTrailerVersion(version) => write!(
                f,
                "unsupported mobile action metadata version {version}; runtime supports {MOBILE_ACTION_VERSION}"
            ),
            Self::TrailerLengthMismatch { declared, actual } => write!(
                f,
                "mobile action metadata length mismatch: declared {declared} bytes, found {actual}"
            ),
            Self::Decode(err) => write!(f, "invalid mobile action metadata JSON: {err}"),
            Self::UnsupportedAbi(abi) => write!(f, "unsupported client action ABI '{abi}'"),
            Self::InvalidAction(err) => write!(f, "invalid client action metadata: {err}"),
        }
    }
}

impl std::error::Error for MobileNbcError {}

impl From<FormatError> for MobileNbcError {
    fn from(value: FormatError) -> Self {
        Self::Base(value)
    }
}

impl CodeModule {
    /// Encode a normal v1 `.nbc` artifact and append validated mobile action
    /// authorization metadata.
    ///
    /// The base bytes are produced by `to_nbc`, so non-mobile runtimes can
    /// still load the artifact unchanged and ignore this additive trailer.
    pub fn to_mobile_nbc(
        &self,
        source_hash: Option<[u8; 32]>,
        mobile_actions: &MobileActionMetadata,
    ) -> Result<Vec<u8>, MobileNbcError> {
        mobile_actions.validate(self)?;
        let mut bytes = self.to_nbc(source_hash)?;
        let payload = serde_json::to_vec(mobile_actions)
            .map_err(|err| MobileNbcError::Decode(err.to_string()))?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            MobileNbcError::InvalidAction("mobile action metadata exceeds u32 length".to_owned())
        })?;
        bytes.extend_from_slice(&MOBILE_ACTION_MAGIC);
        bytes.extend_from_slice(&MOBILE_ACTION_VERSION.to_be_bytes());
        bytes.extend_from_slice(&payload_len.to_be_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }
}

impl MobileNbcArtifact {
    /// Decode a base `.nbc` artifact and, when present, its mobile action
    /// authorization trailer. Plain `.nbc` files decode with an empty table.
    pub fn from_nbc(bytes: &[u8]) -> Result<Self, MobileNbcError> {
        let artifact = CodeModule::from_nbc(bytes)?;
        let base_end = base_artifact_end(bytes)?;

        if bytes.len() == base_end {
            return Ok(Self {
                artifact,
                mobile_actions: MobileActionMetadata::default(),
            });
        }

        let trailer = &bytes[base_end..];
        if trailer.len() < 12 {
            return Err(MobileNbcError::TruncatedTrailer);
        }

        let magic: [u8; 4] = trailer[0..4].try_into().unwrap();
        if magic != MOBILE_ACTION_MAGIC {
            return Err(MobileNbcError::BadTrailerMagic(magic));
        }

        let version = u32::from_be_bytes(trailer[4..8].try_into().unwrap());
        if version != MOBILE_ACTION_VERSION {
            return Err(MobileNbcError::UnsupportedTrailerVersion(version));
        }

        let declared = u32::from_be_bytes(trailer[8..12].try_into().unwrap()) as usize;
        let payload = &trailer[12..];
        if payload.len() != declared {
            return Err(MobileNbcError::TrailerLengthMismatch {
                declared,
                actual: payload.len(),
            });
        }

        let mobile_actions: MobileActionMetadata = serde_json::from_slice(payload)
            .map_err(|err| MobileNbcError::Decode(err.to_string()))?;
        mobile_actions.validate(&artifact.module)?;

        Ok(Self {
            artifact,
            mobile_actions,
        })
    }
}

/// Return the byte offset immediately after the canonical v1 `.nbc` body.
fn base_artifact_end(bytes: &[u8]) -> Result<usize, MobileNbcError> {
    // `CodeModule::from_nbc` has already validated these ranges; keep checked
    // arithmetic here so this helper remains safe if called after future codec
    // changes.
    if bytes.len() < 48 {
        return Err(MobileNbcError::TruncatedTrailer);
    }
    let instr_count = u32::from_be_bytes(bytes[44..48].try_into().unwrap()) as usize;
    let meta_len_off = 48usize
        .checked_add(
            instr_count
                .checked_mul(4)
                .ok_or(MobileNbcError::TruncatedTrailer)?,
        )
        .ok_or(MobileNbcError::TruncatedTrailer)?;
    if bytes.len() < meta_len_off + 4 {
        return Err(MobileNbcError::TruncatedTrailer);
    }
    let meta_len =
        u32::from_be_bytes(bytes[meta_len_off..meta_len_off + 4].try_into().unwrap()) as usize;
    meta_len_off
        .checked_add(4)
        .and_then(|offset| offset.checked_add(meta_len))
        .filter(|end| *end <= bytes.len())
        .ok_or(MobileNbcError::TruncatedTrailer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Instruction, OpCode};

    fn module_with_function() -> CodeModule {
        let mut module = CodeModule::new("mobile-actions");
        module.instructions = vec![Instruction::new0(OpCode::Ret)];
        module.function_table = vec![0];
        module
    }

    fn metadata() -> MobileActionMetadata {
        MobileActionMetadata {
            abi: CLIENT_ACTION_ABI.to_owned(),
            client_actions: vec![ClientActionEntry {
                action_id: "increment".to_owned(),
                handler: "increment".to_owned(),
                function_index: 0,
            }],
        }
    }

    #[test]
    fn mobile_roundtrip_preserves_authorization_table() {
        let module = module_with_function();
        let bytes = module.to_mobile_nbc(None, &metadata()).expect("encode");
        let decoded = MobileNbcArtifact::from_nbc(&bytes).expect("decode");
        assert_eq!(decoded.artifact.module, module);
        assert_eq!(decoded.mobile_actions, metadata());
    }

    #[test]
    fn legacy_decoder_ignores_additive_mobile_trailer() {
        let module = module_with_function();
        let bytes = module.to_mobile_nbc(None, &metadata()).expect("encode");
        let decoded = CodeModule::from_nbc(&bytes).expect("legacy decode");
        assert_eq!(decoded.module, module);
    }

    #[test]
    fn mobile_decoder_accepts_plain_nbc_with_empty_authorization() {
        let module = module_with_function();
        let bytes = module.to_nbc(None).expect("encode");
        let decoded = MobileNbcArtifact::from_nbc(&bytes).expect("decode");
        assert!(decoded.mobile_actions.client_actions.is_empty());
        assert_eq!(decoded.mobile_actions.abi, CLIENT_ACTION_ABI);
    }

    #[test]
    fn duplicate_action_ids_fail_closed() {
        let module = module_with_function();
        let mut meta = metadata();
        meta.client_actions.push(meta.client_actions[0].clone());
        let err = module.to_mobile_nbc(None, &meta).unwrap_err().to_string();
        assert!(err.contains("duplicate action_id"));
    }

    #[test]
    fn out_of_range_function_index_fails_closed() {
        let module = module_with_function();
        let mut meta = metadata();
        meta.client_actions[0].function_index = 1;
        let err = module.to_mobile_nbc(None, &meta).unwrap_err().to_string();
        assert!(err.contains("references function index 1"));
    }

    #[test]
    fn malformed_trailer_length_is_rejected() {
        let module = module_with_function();
        let mut bytes = module.to_mobile_nbc(None, &metadata()).expect("encode");
        bytes.pop();
        let err = MobileNbcArtifact::from_nbc(&bytes).unwrap_err().to_string();
        assert!(err.contains("length mismatch"));
    }

    #[test]
    fn unsupported_mobile_abi_is_rejected() {
        let module = module_with_function();
        let mut meta = metadata();
        meta.abi = "nulang-action-reducer/999".to_owned();
        let err = module.to_mobile_nbc(None, &meta).unwrap_err().to_string();
        assert!(err.contains("unsupported client action ABI"));
    }
}
