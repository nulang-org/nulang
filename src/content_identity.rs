//! Separated content identities for source, semantics, and compiled artifacts.
//!
//! Nulang has historically used content hashes for several different jobs.
//! Those jobs have different invalidation rules, so they should not share one
//! identity:
//!
//! - [`SourceId`] changes when source/package input bytes change.
//! - [`SemanticId`] changes when canonical typed/lowered semantics change.
//! - [`ArtifactId`] changes when target/compiler/backend/codegen inputs change.
//!
//! All three are domain-separated BLAKE3 hashes and intentionally remain
//! additive to existing `.nbc` metadata until a versioned format migration.

use blake3::Hasher;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

const SOURCE_DOMAIN: &[u8] = b"nulang.source-id.v1\0";
const SEMANTIC_DOMAIN: &[u8] = b"nulang.semantic-id.v1\0";
const ARTIFACT_DOMAIN: &[u8] = b"nulang.artifact-id.v1\0";

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn to_hex(self) -> String {
                hex::encode(self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        impl FromStr for $name {
            type Err = ContentIdentityParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(parse_hash(value)?))
            }
        }
    };
}

define_id!(SourceId);
define_id!(SemanticId);
define_id!(ArtifactId);

impl SourceId {
    /// Hash exact source/package input bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(SOURCE_DOMAIN);
        put_bytes(&mut hasher, bytes);
        Self(*hasher.finalize().as_bytes())
    }
}

impl SemanticId {
    /// Hash canonical typed/lowered semantic bytes plus referenced semantic IDs.
    ///
    /// Reference ordering and duplicate declarations are normalized so package
    /// traversal order does not perturb semantic identity. Callers must provide
    /// a canonical semantic representation rather than source pretty text or
    /// backend-specific bytecode.
    pub fn from_canonical_bytes<I>(bytes: &[u8], referenced: I) -> Self
    where
        I: IntoIterator<Item = SemanticId>,
    {
        let refs: BTreeSet<_> = referenced.into_iter().collect();
        let mut hasher = Hasher::new();
        hasher.update(SEMANTIC_DOMAIN);
        put_bytes(&mut hasher, bytes);
        put_u32(&mut hasher, refs.len() as u32);
        for reference in refs {
            hasher.update(reference.as_bytes());
        }
        Self(*hasher.finalize().as_bytes())
    }
}

impl ArtifactId {
    /// Hash one compiled artifact configuration derived from semantic identity.
    ///
    /// Flags are treated as a set: ordering and duplicates do not change the
    /// identity. Only codegen-relevant flags should be supplied here.
    pub fn from_semantic<I, S>(
        semantic: SemanticId,
        compiler_version: &str,
        target: &str,
        abi: &str,
        backend: &str,
        flags: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let flags: BTreeSet<String> = flags
            .into_iter()
            .map(|flag| flag.as_ref().to_string())
            .collect();

        let mut hasher = Hasher::new();
        hasher.update(ARTIFACT_DOMAIN);
        hasher.update(semantic.as_bytes());
        put_bytes(&mut hasher, compiler_version.as_bytes());
        put_bytes(&mut hasher, target.as_bytes());
        put_bytes(&mut hasher, abi.as_bytes());
        put_bytes(&mut hasher, backend.as_bytes());
        put_u32(&mut hasher, flags.len() as u32);
        for flag in flags {
            put_bytes(&mut hasher, flag.as_bytes());
        }
        Self(*hasher.finalize().as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentIdentityParseError {
    InvalidLength(usize),
    InvalidHex(String),
}

impl fmt::Display for ContentIdentityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentIdentityParseError::InvalidLength(length) => {
                write!(f, "content identity must contain 64 hex characters, got {length}")
            }
            ContentIdentityParseError::InvalidHex(value) => {
                write!(f, "content identity is not valid hex: {value}")
            }
        }
    }
}

impl Error for ContentIdentityParseError {}

fn parse_hash(value: &str) -> Result<[u8; 32], ContentIdentityParseError> {
    if value.len() != 64 {
        return Err(ContentIdentityParseError::InvalidLength(value.len()));
    }
    let decoded = hex::decode(value)
        .map_err(|_| ContentIdentityParseError::InvalidHex(value.to_string()))?;
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&decoded);
    Ok(bytes)
}

fn put_u32(hasher: &mut Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn put_bytes(hasher: &mut Hasher, value: &[u8]) {
    put_u32(hasher, value.len() as u32);
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_identity_tracks_exact_input() {
        let first = SourceId::from_bytes(b"fn main() { 1 }");
        let same = SourceId::from_bytes(b"fn main() { 1 }");
        let formatted = SourceId::from_bytes(b"fn main() {\n  1\n}");
        assert_eq!(first, same);
        assert_ne!(first, formatted);
    }

    #[test]
    fn semantic_identity_canonicalizes_reference_order_and_duplicates() {
        let dep_a = SemanticId::from_canonical_bytes(b"dep-a", []);
        let dep_b = SemanticId::from_canonical_bytes(b"dep-b", []);
        let first = SemanticId::from_canonical_bytes(b"core", [dep_a, dep_b]);
        let reordered = SemanticId::from_canonical_bytes(b"core", [dep_b, dep_a, dep_b]);
        assert_eq!(first, reordered);
    }

    #[test]
    fn semantic_identity_changes_with_semantics_or_dependencies() {
        let dep_a = SemanticId::from_canonical_bytes(b"dep-a", []);
        let dep_b = SemanticId::from_canonical_bytes(b"dep-b", []);
        assert_ne!(
            SemanticId::from_canonical_bytes(b"core-a", [dep_a]),
            SemanticId::from_canonical_bytes(b"core-b", [dep_a])
        );
        assert_ne!(
            SemanticId::from_canonical_bytes(b"core", [dep_a]),
            SemanticId::from_canonical_bytes(b"core", [dep_b])
        );
    }

    #[test]
    fn artifact_identity_separates_target_backend_compiler_and_flags() {
        let semantic = SemanticId::from_canonical_bytes(b"core", []);
        let base = ArtifactId::from_semantic(
            semantic,
            "nulangc-0.1.0",
            "x86_64-unknown-linux-gnu",
            "abi-v1",
            "native",
            ["opt=3"],
        );
        let wasm = ArtifactId::from_semantic(
            semantic,
            "nulangc-0.1.0",
            "wasm32-wasi",
            "abi-v1",
            "wasm",
            ["opt=3"],
        );
        let new_compiler = ArtifactId::from_semantic(
            semantic,
            "nulangc-0.2.0",
            "x86_64-unknown-linux-gnu",
            "abi-v1",
            "native",
            ["opt=3"],
        );
        assert_ne!(base, wasm);
        assert_ne!(base, new_compiler);
    }

    #[test]
    fn artifact_flag_order_and_duplicates_are_canonical() {
        let semantic = SemanticId::from_canonical_bytes(b"core", []);
        let first = ArtifactId::from_semantic(
            semantic,
            "compiler",
            "target",
            "abi",
            "backend",
            ["lto", "opt=3"],
        );
        let reordered = ArtifactId::from_semantic(
            semantic,
            "compiler",
            "target",
            "abi",
            "backend",
            ["opt=3", "lto", "lto"],
        );
        assert_eq!(first, reordered);
    }

    #[test]
    fn all_identity_types_round_trip_hex() {
        let source = SourceId::from_bytes(b"source");
        let semantic = SemanticId::from_canonical_bytes(b"semantic", []);
        let artifact = ArtifactId::from_semantic(
            semantic,
            "compiler",
            "target",
            "abi",
            "backend",
            std::iter::empty::<&str>(),
        );
        assert_eq!(source.to_string().parse::<SourceId>().unwrap(), source);
        assert_eq!(semantic.to_string().parse::<SemanticId>().unwrap(), semantic);
        assert_eq!(artifact.to_string().parse::<ArtifactId>().unwrap(), artifact);
        assert!("abcd".parse::<ArtifactId>().is_err());
    }
}
