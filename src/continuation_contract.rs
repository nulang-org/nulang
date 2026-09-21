//! Backend-independent continuation identity and resume compatibility.
//!
//! VM frames, bytecode program counters, Cranelift registers, and native stack
//! layouts are execution artifacts. Durable/debuggable suspension points need a
//! smaller semantic contract that can survive backend changes and can reject an
//! unsafe hot patch before any machine state is resumed.

use crate::content_identity::SemanticId;
use blake3::Hasher;
use std::fmt;

const POINT_DOMAIN: &[u8] = b"nulang.continuation-point.v1\0";
const LAYOUT_DOMAIN: &[u8] = b"nulang.continuation-layout.v1\0";
const WIRE_MAGIC: [u8; 4] = *b"NUCT";
const WIRE_VERSION: u16 = 1;
const WIRE_LEN: usize = 4 + 2 + 32 + 32 + 32 + 8 + 8 + 4;

fn put_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u32).to_le_bytes());
    hasher.update(bytes);
}

/// Compiler-owned kind of logical suspension boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ContinuationBoundary {
    Effect = 1,
    Receive = 2,
    Await = 3,
    Timer = 4,
    WorkflowCheckpoint = 5,
    DebugSafepoint = 6,
}

/// Stable logical identity of a suspension site.
///
/// The identity deliberately excludes bytecode/native PCs and the whole-program
/// SemanticId. A replacement build may therefore resume an older suspended
/// activation when it preserves the same nominal site and live-layout contract.
///
/// The initial compiler integration may use a deterministic suspension ordinal
/// within a canonical qualified callable. If a preceding suspension site is
/// inserted, the ordinal changes and compatibility fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContinuationPointId([u8; 32]);

impl ContinuationPointId {
    pub fn derive(
        definition_name: &str,
        callable_name: &str,
        boundary: ContinuationBoundary,
        suspension_ordinal: u32,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(POINT_DOMAIN);
        put_bytes(&mut hasher, definition_name.as_bytes());
        put_bytes(&mut hasher, callable_name.as_bytes());
        hasher.update(&[boundary as u8]);
        hasher.update(&suspension_ordinal.to_le_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn from_digest_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Hash of the values that must be materializable at a continuation point.
///
/// The compiler owns the canonical layout bytes. They should describe semantic
/// live locals/temporaries, their canonical types, resume-value type, and any
/// handler state needed to continue execution; they must not contain register
/// numbers, stack offsets, or backend-specific representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContinuationLayoutId([u8; 32]);

impl ContinuationLayoutId {
    pub fn derive(
        point: ContinuationPointId,
        resume_type: &str,
        canonical_live_layout: &[u8],
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(LAYOUT_DOMAIN);
        hasher.update(point.as_bytes());
        put_bytes(&mut hasher, resume_type.as_bytes());
        put_bytes(&mut hasher, canonical_live_layout);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn from_digest_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Semantic contract attached to one captured logical continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuationContract {
    /// Exact code semantics that originally captured the continuation.
    pub code_semantic_id: SemanticId,
    /// Stable logical suspension site.
    pub point_id: ContinuationPointId,
    /// Portable live-value/resume ABI at that site.
    pub layout_id: ContinuationLayoutId,
}

impl ContinuationContract {
    /// Check whether suspended state captured by `self` may resume in
    /// `replacement`.
    ///
    /// The code SemanticId is intentionally not compared: hot patching changes
    /// code semantics by definition. Resume is allowed only when the compiler
    /// proves that the logical suspension site and its portable live layout are
    /// unchanged.
    pub fn check_resume_compatibility(
        &self,
        replacement: &Self,
    ) -> Result<(), ContinuationCompatibilityError> {
        if self.point_id != replacement.point_id {
            return Err(ContinuationCompatibilityError::PointChanged);
        }
        if self.layout_id != replacement.layout_id {
            return Err(ContinuationCompatibilityError::LayoutChanged);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationCompatibilityError {
    PointChanged,
    LayoutChanged,
}

impl fmt::Display for ContinuationCompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PointChanged => write!(f, "continuation point changed"),
            Self::LayoutChanged => write!(f, "continuation live layout changed"),
        }
    }
}

impl std::error::Error for ContinuationCompatibilityError {}

/// Pointer-free metadata persisted beside a portable continuation payload.
///
/// This first slice intentionally stores no VM frame bytes. It is the stable
/// compatibility envelope that future portable local-value serialization,
/// actor snapshots, debugger replay, and Cloud migration can share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortableContinuationMetadata {
    pub contract: ContinuationContract,
    pub activation_sequence: u64,
    pub message_sequence: u64,
    pub effect_cursor: u32,
}

impl PortableContinuationMetadata {
    pub fn encode(self) -> [u8; WIRE_LEN] {
        let mut out = [0u8; WIRE_LEN];
        out[0..4].copy_from_slice(&WIRE_MAGIC);
        out[4..6].copy_from_slice(&WIRE_VERSION.to_be_bytes());
        out[6..38].copy_from_slice(self.contract.code_semantic_id.as_bytes());
        out[38..70].copy_from_slice(self.contract.point_id.as_bytes());
        out[70..102].copy_from_slice(self.contract.layout_id.as_bytes());
        out[102..110].copy_from_slice(&self.activation_sequence.to_be_bytes());
        out[110..118].copy_from_slice(&self.message_sequence.to_be_bytes());
        out[118..122].copy_from_slice(&self.effect_cursor.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PortableContinuationDecodeError> {
        if bytes.len() != WIRE_LEN {
            return Err(PortableContinuationDecodeError::InvalidLength {
                actual: bytes.len(),
            });
        }
        if bytes[0..4] != WIRE_MAGIC {
            return Err(PortableContinuationDecodeError::InvalidMagic);
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != WIRE_VERSION {
            return Err(PortableContinuationDecodeError::UnsupportedVersion {
                actual: version,
            });
        }

        let code_semantic_id = SemanticId::from_digest_bytes(
            bytes[6..38].try_into().expect("validated fixed semantic id length"),
        );
        let point_id = ContinuationPointId::from_digest_bytes(
            bytes[38..70].try_into().expect("validated fixed point id length"),
        );
        let layout_id = ContinuationLayoutId::from_digest_bytes(
            bytes[70..102].try_into().expect("validated fixed layout id length"),
        );

        Ok(Self {
            contract: ContinuationContract {
                code_semantic_id,
                point_id,
                layout_id,
            },
            activation_sequence: u64::from_be_bytes(
                bytes[102..110].try_into().expect("validated activation sequence"),
            ),
            message_sequence: u64::from_be_bytes(
                bytes[110..118].try_into().expect("validated message sequence"),
            ),
            effect_cursor: u32::from_be_bytes(
                bytes[118..122].try_into().expect("validated effect cursor"),
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableContinuationDecodeError {
    InvalidLength { actual: usize },
    InvalidMagic,
    UnsupportedVersion { actual: u16 },
}

impl fmt::Display for PortableContinuationDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { actual } => {
                write!(f, "portable continuation metadata must be {WIRE_LEN} bytes, got {actual}")
            }
            Self::InvalidMagic => write!(f, "invalid portable continuation metadata magic"),
            Self::UnsupportedVersion { actual } => write!(
                f,
                "unsupported portable continuation metadata version {actual}"
            ),
        }
    }
}

impl std::error::Error for PortableContinuationDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic(bytes: &[u8]) -> SemanticId {
        SemanticId::from_canonical_bytes(bytes, [])
    }

    fn contract(code: &[u8], layout: &[u8]) -> ContinuationContract {
        let point = ContinuationPointId::derive(
            "payments.OrderWorkflow",
            "charge",
            ContinuationBoundary::Effect,
            0,
        );
        ContinuationContract {
            code_semantic_id: semantic(code),
            point_id: point,
            layout_id: ContinuationLayoutId::derive(point, "Result<Receipt, Error>", layout),
        }
    }

    #[test]
    fn logical_point_excludes_backend_and_whole_program_identity() {
        let first = ContinuationPointId::derive(
            "payments.OrderWorkflow",
            "charge",
            ContinuationBoundary::Effect,
            0,
        );
        let same = ContinuationPointId::derive(
            "payments.OrderWorkflow",
            "charge",
            ContinuationBoundary::Effect,
            0,
        );
        let later = ContinuationPointId::derive(
            "payments.OrderWorkflow",
            "charge",
            ContinuationBoundary::Effect,
            1,
        );
        assert_eq!(first, same);
        assert_ne!(first, later);
    }

    #[test]
    fn code_patch_can_resume_when_point_and_layout_are_unchanged() {
        let captured = contract(b"old-code", b"amount:Int;order:OrderId");
        let replacement = contract(b"new-code", b"amount:Int;order:OrderId");
        assert_ne!(captured.code_semantic_id, replacement.code_semantic_id);
        assert_eq!(captured.check_resume_compatibility(&replacement), Ok(()));
    }

    #[test]
    fn changed_live_layout_fails_closed() {
        let captured = contract(b"old-code", b"amount:Int");
        let replacement = contract(b"new-code", b"amount:Int;currency:String");
        assert_eq!(
            captured.check_resume_compatibility(&replacement),
            Err(ContinuationCompatibilityError::LayoutChanged)
        );
    }

    #[test]
    fn moved_logical_suspension_site_fails_closed() {
        let captured = contract(b"old-code", b"amount:Int");
        let point = ContinuationPointId::derive(
            "payments.OrderWorkflow",
            "charge",
            ContinuationBoundary::Effect,
            1,
        );
        let replacement = ContinuationContract {
            code_semantic_id: semantic(b"new-code"),
            point_id: point,
            layout_id: ContinuationLayoutId::derive(point, "Result<Receipt, Error>", b"amount:Int"),
        };
        assert_eq!(
            captured.check_resume_compatibility(&replacement),
            Err(ContinuationCompatibilityError::PointChanged)
        );
    }

    #[test]
    fn portable_metadata_round_trips_without_vm_or_native_state() {
        let metadata = PortableContinuationMetadata {
            contract: contract(b"code", b"amount:Int"),
            activation_sequence: 17,
            message_sequence: 44,
            effect_cursor: 3,
        };
        let bytes = metadata.encode();
        assert_eq!(bytes.len(), WIRE_LEN);
        assert_eq!(PortableContinuationMetadata::decode(&bytes).unwrap(), metadata);
    }

    #[test]
    fn portable_metadata_rejects_unknown_versions_and_trailing_bytes() {
        let metadata = PortableContinuationMetadata {
            contract: contract(b"code", b"amount:Int"),
            activation_sequence: 1,
            message_sequence: 2,
            effect_cursor: 0,
        };
        let mut bytes = metadata.encode();
        bytes[4..6].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            PortableContinuationMetadata::decode(&bytes),
            Err(PortableContinuationDecodeError::UnsupportedVersion { actual: 2 })
        );

        let mut oversized = metadata.encode().to_vec();
        oversized.push(0);
        assert_eq!(
            PortableContinuationMetadata::decode(&oversized),
            Err(PortableContinuationDecodeError::InvalidLength {
                actual: WIRE_LEN + 1
            })
        );
    }
}
