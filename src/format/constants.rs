//! Canonical, frozen format-version constants for Nulang's durable formats.
//!
//! This module is the **single source of truth** for the magic bytes and
//! version numbers that identify Nulang's on-disk bytecode (`.nbc`) and the
//! NUL0 wire protocol. A format version is *frozen* the moment it is assigned:
//! the byte layout for that version never changes. Additive extensions mint a
//! new version and a migration in [`crate::format::migrate`]; existing
//! versions are never silently reinterpreted. See `SPEC2.md` §"Format
//! Stability" for the stability contract.
//!
//! # Why this matters
//!
//! Every long-lived binary format (ELF, PE, WAV, PNG, the Java classfile) has
//! a magic + version header and a documented stability contract. A Nulang
//! program compiled in 2026 must be loadable by a conforming runtime in 2126;
//! that requires the runtime to *recognise* the artifact's version and refuse
//! — never reinterpret — a format it does not understand.

/// Magic bytes prefixing every `.nbc` (Nulang Bytecode) artifact: ASCII `"NLBC"`.
pub const BYTECODE_MAGIC: [u8; 4] = *b"NLBC";

/// Current `.nbc` format version. Version 1 is the initial frozen layout
/// (binary header + binary instruction stream + JSON metadata body; see
/// [`crate::format::nbc`] for the byte layout).
pub const BYTECODE_VERSION: u32 = 1;

/// Highest `.nbc` format version this runtime can read. A artifact whose
/// `format_version` exceeds this is rejected with
/// [`FormatError::UnsupportedVersion`] rather than reinterpreted.
pub const BYTECODE_MAX_VERSION: u32 = 1;

/// Magic bytes prefixing the NUL0 wire-protocol handshake: ASCII `"NUL0"`.
/// Also prefixes every framed packet payload (see `src/runtime/network.rs`).
pub const WIRE_MAGIC: [u8; 4] = *b"NUL0";

/// Current NUL0 wire-protocol version. Version 1 is the initial layout:
/// 16-byte handshake `{magic, version:u32, node_id:u64}` followed by the
/// existing length-prefixed packet framing.
pub const WIRE_VERSION: u32 = 1;

/// Value-layout version — pins the i64-tagged representation in
/// [`crate::value_layout`]. Bumping this is a breaking change to every
/// compiled artifact and every wire peer; it requires an RFC.
pub const VALUE_LAYOUT_VERSION: u32 = 1;

/// Nulang *language* version, recorded in every `.nbc` artifact. This is
/// distinct from the crate (`Cargo.toml`) version: the crate may rev freely,
/// but the language version moves only on RFC-ratified change. See
/// `CHANGELOG.md` and `GOVERNANCE.md`.
pub const LANGUAGE_VERSION: u32 = 2;
/// String form of [`LANGUAGE_VERSION`] for telemetry and CLI output.
pub const LANGUAGE_VERSION_STR: &str = "2.0.0-dev";

/// Length of a `.nbc` file header in bytes (magic + format_version +
/// language_version + source_hash + instr_count).
pub const NBC_HEADER_LEN: usize = 4 + 4 + 4 + 32 + 4; // = 48

/// Length of the NUL0 wire handshake in bytes (magic + version + node_id).
pub const WIRE_HANDSHAKE_LEN: usize = 4 + 4 + 8; // = 16

/// Error raised when a durable format artifact cannot be loaded.
///
/// Every variant corresponds to a *recognised* failure mode — never to silent
/// reinterpretation. A runtime that receives an artifact it cannot understand
/// MUST surface one of these rather than guessing at the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// Input had fewer bytes than the header requires.
    Truncated { need: usize, have: usize },
    /// Magic bytes did not match the expected format identifier.
    BadMagic { expected: [u8; 4], got: [u8; 4] },
    /// Artifact format version is higher than this runtime understands.
    UnsupportedVersion { max_supported: u32, found: u32 },
    /// Artifact language version is incompatible with this runtime.
    IncompatibleLanguage { runtime: u32, artifact: u32 },
    /// Declared body length did not match the bytes actually present.
    LengthMismatch { declared: u32, actual: usize },
    /// An instruction stream contained an unknown opcode value. This is the
    /// "opcode values are never reused" guarantee in action: an artifact
    /// minted by a newer runtime that uses an opcode this runtime does not
    /// know is rejected, not misinterpreted.
    UnknownOpcode { opcode: u8 },
    /// The JSON metadata body failed to deserialize.
    BodyDecode(String),
    /// An arithmetic constant could not be represented (e.g. non-finite float).
    BadConstant(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Truncated { need, have } => write!(
                f, "format error: truncated header, need {need} bytes, have {have}"
            ),
            FormatError::BadMagic { expected, got } => write!(
                f, "format error: bad magic, expected {:?}, got {:?}", expected, got
            ),
            FormatError::UnsupportedVersion { max_supported, found } => write!(
                f, "format error: artifact format version {found} exceeds this runtime's max {max_supported}"
            ),
            FormatError::IncompatibleLanguage { runtime, artifact } => write!(
                f, "format error: artifact language version {artifact} incompatible with runtime language version {runtime}"
            ),
            FormatError::LengthMismatch { declared, actual } => write!(
                f, "format error: declared length {declared} but {actual} bytes present"
            ),
            FormatError::UnknownOpcode { opcode } => write!(
                f, "format error: unknown opcode 0x{opcode:02X} in instruction stream"
            ),
            FormatError::BodyDecode(msg) => write!(f, "format error: metadata body decode failed: {msg}"),
            FormatError::BadConstant(msg) => write!(f, "format error: bad constant: {msg}"),
        }
    }
}

impl std::error::Error for FormatError {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_language_version_str() {
        assert!(
            LANGUAGE_VERSION_STR.starts_with(&LANGUAGE_VERSION.to_string()),
            "String version should match numeric"
        );
    }

    #[test]
    fn test_magic_bytes_are_ascii() {
        assert_eq!(&BYTECODE_MAGIC, b"NLBC");
        assert_eq!(&WIRE_MAGIC, b"NUL0");
    }

    #[test]
    fn test_version_constants_are_frozen_v1() {
        // These are part of the Frozen tier (CHANGELOG.md). Editing them is
        // a breaking change requiring an RFC.
        assert_eq!(BYTECODE_VERSION, 1);
        assert_eq!(BYTECODE_MAX_VERSION, 1);
        assert_eq!(WIRE_VERSION, 1);
        assert_eq!(VALUE_LAYOUT_VERSION, 1);
        assert_eq!(LANGUAGE_VERSION, 1);
    }

    #[test]
    fn test_header_lengths() {
        assert_eq!(NBC_HEADER_LEN, 48);
        assert_eq!(WIRE_HANDSHAKE_LEN, 16);
    }

    #[test]
    fn test_format_error_display_mentions_version() {
        let e = FormatError::UnsupportedVersion {
            max_supported: 1,
            found: 2,
        };
        let s = format!("{e}");
        assert!(s.contains("version 2") && s.contains("max 1"), "got: {s}");
    }
}
