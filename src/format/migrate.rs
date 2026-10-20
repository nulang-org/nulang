//! Format migration registry.
//!
//! This is the **only** legal place where one `.nbc` format version is
//! converted into another. Version 1 is frozen; no migration is defined yet
//! beyond the identity `v1 -> v1`. When a v2 format is introduced (RFC
//! required), a `v1_to_v2` step is registered here and `migrate_nbc` walks the
//! chain `v1 -> v2 -> ... -> target` so a runtime that speaks v2 can load a
//! v1 artifact by transparently running the registered migrations.
//!
//! # Stability contract
//!
//! A migration is a pure function over bytes: it MUST be deterministic, total
//! over its declared input version, and preserve the meaning of the encoded
//! module. Migrations are append-only — once a `vN_to_v(N+1)` step is
//! published it never changes (a changed migration would silently invalidate
//! every artifact already upgraded through it). New steps are added; existing
//! steps are immutable.

use crate::format::constants::{FormatError, BYTECODE_MAGIC, NBC_HEADER_LEN};

/// The list of format versions this runtime can migrate *between* (inclusive).
/// Today only v1 exists, so the only legal migration is the identity v1 -> v1.
const KNOWN_VERSIONS: &[u32] = &[1];

/// Read the `format_version` field from a `.nbc` header without decoding the
/// body. Returns `None` if the bytes are too short to contain a header or the
/// magic does not match (i.e. the input is not a `.nbc` artifact at all).
pub fn peek_format_version(bytes: &[u8]) -> Option<u32> {
    if bytes.len() < NBC_HEADER_LEN {
        return None;
    }
    if &bytes[0..4] != BYTECODE_MAGIC.as_slice() {
        return None;
    }
    Some(u32::from_be_bytes(bytes[4..8].try_into().unwrap()))
}

/// Migrate a `.nbc` artifact from its recorded format version up to
/// `target_version`, applying each registered step in order.
///
/// For now the only registered step is the identity `v1 -> v1`, so any other
/// request fails with [`FormatError::UnsupportedVersion`]. When v2 lands,
/// add a `v1_to_v2` step here and this function will walk `v1 -> v2`
/// automatically.
pub fn migrate_nbc(bytes: &[u8], target_version: u32) -> Result<Vec<u8>, FormatError> {
    let from = match peek_format_version(bytes) {
        Some(version) => version,
        None if bytes.len() < NBC_HEADER_LEN => {
            return Err(FormatError::Truncated {
                need: NBC_HEADER_LEN,
                have: bytes.len(),
            });
        }
        None => {
            return Err(FormatError::BadMagic {
                expected: BYTECODE_MAGIC,
                got: bytes[0..4].try_into().expect("complete header"),
            });
        }
    };
    if !KNOWN_VERSIONS.contains(&from) || !KNOWN_VERSIONS.contains(&target_version) {
        return Err(FormatError::UnsupportedVersion {
            max_supported: *KNOWN_VERSIONS.last().unwrap(),
            found: if !KNOWN_VERSIONS.contains(&from) {
                from
            } else {
                target_version
            },
        });
    }
    // v1 -> v1: preserve the original bytes, but only after validating
    // that the artifact is a complete, loadable v1 module. A version header
    // alone does not prove that the instruction stream or metadata is valid.
    // In particular, do not return success for truncated or corrupt history.
    if from == 1 && target_version == 1 {
        crate::bytecode::CodeModule::from_nbc(bytes)?;
        return Ok(bytes.to_vec());
    }
    Err(FormatError::UnsupportedVersion {
        max_supported: *KNOWN_VERSIONS.last().unwrap(),
        found: target_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};

    #[test]
    fn test_peek_format_version_reads_header() {
        let bytes = CodeModule::new("t").to_nbc(None).unwrap();
        assert_eq!(peek_format_version(&bytes), Some(1));
    }

    #[test]
    fn test_peek_format_version_rejects_non_nbc() {
        assert_eq!(peek_format_version(b"nope"), None);
    }

    #[test]
    fn test_migrate_identity_v1_to_v1() {
        let mut m = CodeModule::new("t");
        m.add_constant(Constant::Int(7));
        m.emit(Instruction::new0(OpCode::Halt));
        let bytes = m.to_nbc(None).unwrap();
        let migrated = migrate_nbc(&bytes, 1).expect("v1->v1 identity");
        assert_eq!(migrated, bytes, "identity migration must be byte-identical");
    }

    #[test]
    fn identity_migration_reports_bad_magic_not_truncation() {
        let mut bytes = CodeModule::new("t").to_nbc(None).unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            migrate_nbc(&bytes, 1),
            Err(FormatError::BadMagic { .. })
        ));
    }

    #[test]
    fn identity_migration_rejects_missing_metadata_body() {
        // A valid versioned header alone is not a valid .nbc artifact.
        // Previously migrate_nbc returned Ok for this truncated input.
        let mut bytes = CodeModule::new("t").to_nbc(None).unwrap();
        bytes.truncate(NBC_HEADER_LEN);
        assert!(matches!(
            migrate_nbc(&bytes, 1),
            Err(FormatError::Truncated { .. })
        ));
    }

    #[test]
    fn identity_migration_rejects_unknown_opcode() {
        let mut module = CodeModule::new("t");
        module.emit(Instruction::new0(OpCode::Halt));
        let mut bytes = module.to_nbc(None).unwrap();
        bytes[NBC_HEADER_LEN] = 0xFE;
        assert!(matches!(
            migrate_nbc(&bytes, 1),
            Err(FormatError::UnknownOpcode { opcode: 0xFE })
        ));
    }

    #[test]
    fn identity_migration_rejects_corrupt_metadata() {
        let bytes = CodeModule::new("t").to_nbc(None).unwrap();
        let mut corrupt = bytes;
        // The encoded body must be valid JSON, not just have a plausible length.
        let last = corrupt.last_mut().expect("nonempty metadata");
        *last = b'!';
        assert!(matches!(
            migrate_nbc(&corrupt, 1),
            Err(FormatError::BodyDecode(_))
        ));
    }

    #[test]
    fn test_migrate_rejects_unknown_versions() {
        let bytes = CodeModule::new("t").to_nbc(None).unwrap();
        let err = migrate_nbc(&bytes, 2).unwrap_err();
        assert!(matches!(err, FormatError::UnsupportedVersion { .. }));
    }
}
